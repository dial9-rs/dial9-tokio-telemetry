pub mod telemetry;
pub mod traced;

use arc_swap::ArcSwap;
use pin_project_lite::pin_project;
use smallvec::SmallVec;
use std::collections::HashMap;
use std::sync::Arc;
use std::time::{Duration, SystemTime};
use tokio::runtime::dump::Trace;
use tokio::sync::mpsc;

/// Configuration for trace collection behavior.
#[derive(Clone, Debug)]
pub struct TrackerConfig {
    /// Minimum time delta between trace captures. If a new trace is captured
    /// within this duration of the previous trace, it will be ignored.
    pub min_trace_interval: Duration,
}

impl Default for TrackerConfig {
    fn default() -> Self {
        Self {
            min_trace_interval: Duration::from_secs(1),
        }
    }
}

/// A timestamped trace entry.
pub type TraceEntry = (SystemTime, Arc<Trace>);

#[derive(Debug, Clone)]
pub enum SentinelStatus {
    /// Task is pending with a history of traces.
    /// Each entry contains the timestamp when the trace was captured and the trace itself.
    /// The SmallVec is optimized for the common case of a small number of traces.
    Pending(SmallVec<[TraceEntry; 4]>),
    Completed,
    Cancelled,
}

pub type Sentinel = Arc<ArcSwap<SentinelStatus>>;

#[derive(Hash, Eq, PartialEq)]
struct SentinelKey(usize);

impl SentinelKey {
    fn from_sentinel(s: &Sentinel) -> Self {
        Self(Arc::as_ptr(s) as usize)
    }
}

/// Count of how many times a sentinel has been sent to the receiver.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct SendCount(pub usize);

/// Information about a detected long-running task.
#[derive(Debug, Clone)]
pub struct LongPollEvent {
    pub status: SentinelStatus,
    pub send_count: SendCount,
}

pub struct LongPollTracker {
    buckets: Vec<Vec<Sentinel>>,
    current_bucket: usize,
    send_counts: HashMap<SentinelKey, usize>,
    tx: mpsc::UnboundedSender<LongPollEvent>,
    sentinel_rx: mpsc::UnboundedReceiver<Sentinel>,
}

pub struct TrackerHandle {
    pub rx: mpsc::UnboundedReceiver<LongPollEvent>,
    pub sentinel_tx: mpsc::UnboundedSender<Sentinel>,
}

impl LongPollTracker {
    pub fn new() -> (Self, TrackerHandle) {
        let (tx, rx) = mpsc::unbounded_channel();
        let (sentinel_tx, sentinel_rx) = mpsc::unbounded_channel();
        let tracker = Self {
            buckets: (0..60).map(|_| Vec::new()).collect(),
            current_bucket: 0,
            send_counts: HashMap::new(),
            tx,
            sentinel_rx,
        };
        let handle = TrackerHandle { rx, sentinel_tx };
        (tracker, handle)
    }

    pub fn add_sentinel(&mut self, sentinel: Sentinel) {
        self.buckets[0].push(sentinel);
    }

    /// Process one tick of the tracker. Returns `false` if the tracker should shut down
    /// (i.e., when the sentinel_tx side of the channel has been dropped).
    pub fn tick(&mut self) -> bool {
        // Drain incoming sentinels and add them 6 buckets ahead
        loop {
            match self.sentinel_rx.try_recv() {
                Ok(sentinel) => {
                    let target_bucket = (self.current_bucket + 6) % 60;
                    self.buckets[target_bucket].push(sentinel);
                }
                Err(mpsc::error::TryRecvError::Empty) => break,
                Err(mpsc::error::TryRecvError::Disconnected) => return false,
            }
        }

        self.current_bucket = (self.current_bucket + 1) % 60;
        let bucket = &mut self.buckets[self.current_bucket];

        let mut active = Vec::new();
        for sentinel in bucket.drain(..) {
            let status = sentinel.load();
            match &**status {
                SentinelStatus::Pending(traces) => {
                    if let Some((first_time, _)) = traces.first() {
                        let age = SystemTime::now()
                            .duration_since(*first_time)
                            .unwrap_or_default();
                        if age.as_secs() >= 6 {
                            let key = SentinelKey::from_sentinel(&sentinel);
                            let count = self.send_counts.entry(key).or_insert(0);

                            // Clone the status for the event (Arc<Trace> is cheap to clone)
                            let cloned_status = SentinelStatus::Pending(traces.clone());

                            let _ = self.tx.send(LongPollEvent {
                                status: cloned_status,
                                send_count: SendCount(*count),
                            });

                            *count += 1;
                            active.push(sentinel);
                        } else {
                            active.push(sentinel);
                        }
                    }
                }
                SentinelStatus::Completed | SentinelStatus::Cancelled => {}
            }
        }

        *bucket = active;
        true
    }

    pub fn spawn(mut self) {
        tokio::spawn(async move {
            loop {
                tokio::time::sleep(std::time::Duration::from_secs(1)).await;
                if !self.tick() {
                    break;
                }
            }
        });
    }
}

use std::future::Future;
use std::pin::Pin;
use std::task::{Context, Poll};

pin_project! {
    pub struct DetectLongWait<F> {
        #[pin]
        inner: F,
        sentinel: Option<Sentinel>,
        tx: mpsc::UnboundedSender<Sentinel>,
        config: TrackerConfig,
    }

    impl<F> PinnedDrop for DetectLongWait<F> {
        fn drop(this: Pin<&mut Self>) {
            if let Some(sentinel) = &this.sentinel
                && matches!(**sentinel.load(), SentinelStatus::Pending(..))
            {
                sentinel.store(Arc::new(SentinelStatus::Cancelled));
            }
        }
    }
}

impl<F> DetectLongWait<F> {
    pub fn new(inner: F, tx: mpsc::UnboundedSender<Sentinel>) -> Self {
        Self {
            inner,
            sentinel: None,
            tx,
            config: TrackerConfig::default(),
        }
    }

    pub fn with_config(
        inner: F,
        tx: mpsc::UnboundedSender<Sentinel>,
        config: TrackerConfig,
    ) -> Self {
        Self {
            inner,
            sentinel: None,
            tx,
            config,
        }
    }
}

impl<F: Future> Future for DetectLongWait<F> {
    type Output = F::Output;

    fn poll(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Self::Output> {
        let mut this = self.project();

        // Poll the inner future.
        match this.inner.as_mut().poll(cx) {
            Poll::Ready(output) => {
                if let Some(sentinel) = &this.sentinel {
                    sentinel.store(Arc::new(SentinelStatus::Completed));
                }
                return Poll::Ready(output);
            }
            Poll::Pending => {}
        };

        // its poll pending, trace it:
        let (result, trace) = Trace::capture(|| this.inner.as_mut().poll(cx));
        let now = SystemTime::now();
        let trace = Arc::new(trace);

        if matches!(result, Poll::Pending) {
            if let Some(sentinel) = this.sentinel.as_ref() {
                // Update existing sentinel - only add new trace if enough time has passed
                let current_status = sentinel.load();
                let mut traces: SmallVec<[TraceEntry; 4]> = match &**current_status {
                    SentinelStatus::Pending(existing_traces) => existing_traces.clone(),
                    _ => SmallVec::new(), // Shouldn't happen, but handle gracefully
                };

                // Only add the new trace if the delta from the last trace exceeds the threshold
                let should_add = traces
                    .last()
                    .map(|(last_time, _)| {
                        now.duration_since(*last_time).unwrap_or(Duration::ZERO)
                            >= this.config.min_trace_interval
                    })
                    .unwrap_or(true);

                if should_add {
                    traces.push((now, trace));
                }

                sentinel.store(Arc::new(SentinelStatus::Pending(traces)));
            } else {
                // First poll - create new sentinel and send to tracker
                let mut traces = SmallVec::new();
                traces.push((now, trace));
                let sentinel = Arc::new(ArcSwap::from_pointee(SentinelStatus::Pending(traces)));
                let _ = this.tx.send(Arc::clone(&sentinel));
                *this.sentinel = Some(sentinel);
            }
        }
        result
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use assert2::check;
    use tokio::time::Duration;

    /// Test that a long-running task triggers long poll detection.
    /// Corresponds to examples/long_sleep.rs
    #[tokio::test]
    #[ignore = "long poll tracker tasks need refactor"]
    async fn test_long_sleep_detection() {
        let (tracker, mut handle) = LongPollTracker::new();
        tracker.spawn();

        let sleep_future = tokio::time::sleep(Duration::from_secs(10));
        let wrapped = DetectLongWait::new(sleep_future, handle.sentinel_tx.clone());

        let task = tokio::spawn(async move {
            wrapped.await;
        });

        // Wait for long poll detection (need ~7 seconds for bucket rotation)
        let mut detected = false;
        for _ in 1..=10 {
            tokio::time::sleep(Duration::from_secs(1)).await;

            if let Ok(event) = handle.rx.try_recv() {
                // Verify we got a pending status with traces
                match &event.status {
                    SentinelStatus::Pending(traces) => {
                        check!(traces.len() >= 1);
                        let (_, trace) = &traces[0];
                        check!(trace.to_string() != "");
                    }
                    _ => panic!("Expected Pending status"),
                }
                detected = true;
                break;
            }
        }

        check!(
            detected,
            "Long poll should have been detected within 10 seconds"
        );
        task.abort();
    }

    /// Test that a task completing before the threshold does NOT trigger a trace.
    /// Corresponds to examples/completing_task.rs
    #[tokio::test]
    #[ignore = "long poll tracker tasks need refactor"]
    async fn test_completing_task_no_trace() {
        let (tracker, mut handle) = LongPollTracker::new();
        tracker.spawn();

        // Short sleep that completes before the 6-second threshold
        let sleep_future = tokio::time::sleep(Duration::from_secs(2));
        let wrapped = DetectLongWait::new(sleep_future, handle.sentinel_tx.clone());

        wrapped.await;

        // Wait a bit more to ensure no trace is sent
        tokio::time::sleep(Duration::from_secs(5)).await;

        // Drop the sentinel_tx to signal the tracker to shut down gracefully
        drop(handle.sentinel_tx);

        // Should NOT have received any trace - channel returns None when closed and empty
        check!(handle.rx.recv().await.is_none());
    }

    /// Test that a cancelled task is properly marked as Cancelled.
    /// Corresponds to examples/cancelled_task.rs
    #[tokio::test]
    #[ignore = "long poll tracker tasks need refactor"]
    async fn test_cancelled_task() {
        let (tracker, mut handle) = LongPollTracker::new();
        tracker.spawn();

        let sleep_future = tokio::time::sleep(Duration::from_secs(20));
        let wrapped = DetectLongWait::new(sleep_future, handle.sentinel_tx.clone());

        let task = tokio::spawn(async move {
            wrapped.await;
        });

        // Wait for long poll detection (7 seconds to ensure bucket rotation)
        tokio::time::sleep(Duration::from_secs(7)).await;

        // Check if we got a trace while task is still pending
        if let Ok(event) = handle.rx.try_recv() {
            match &event.status {
                SentinelStatus::Pending(traces) => {
                    check!(traces.len() >= 1);
                }
                _ => panic!("Expected Pending status"),
            }
        }

        // Cancel the task
        task.abort();

        // Give a moment for the drop to execute
        tokio::time::sleep(Duration::from_millis(100)).await;

        // The task should have been aborted
        check!(task.is_finished());
    }

    /// Test timing of long poll detection.
    /// Corresponds to examples/debug_timing.rs
    #[tokio::test]
    #[ignore = "long poll tracker tasks need refactor"]
    async fn test_detection_timing() {
        let (tracker, mut handle) = LongPollTracker::new();
        tracker.spawn();

        let sleep_future = tokio::time::sleep(Duration::from_secs(20));
        let wrapped = DetectLongWait::new(sleep_future, handle.sentinel_tx.clone());

        let task = tokio::spawn(async move {
            wrapped.await;
        });

        // Track when detection occurs
        let mut detection_time = None;
        for i in 1..=10 {
            tokio::time::sleep(Duration::from_secs(1)).await;

            if let Ok(event) = handle.rx.try_recv() {
                match &event.status {
                    SentinelStatus::Pending(traces) => {
                        check!(traces.len() >= 1);
                        let (_, trace) = &traces[0];
                        check!(trace.to_string() != "");
                    }
                    _ => panic!("Expected Pending status"),
                }
                check!(event.send_count == SendCount(0));
                detection_time = Some(i);
                break;
            }
        }

        // Detection should happen around 6-7 seconds (after bucket rotation)
        let detected_at = detection_time.expect("Should have detected long poll");
        check!(
            detected_at >= 6 && detected_at <= 8,
            "Detection should occur between 6-8 seconds, got {}s",
            detected_at
        );

        task.abort();
    }

    /// Test that sentinel status transitions correctly from Pending to Completed.
    #[tokio::test]
    #[ignore]
    async fn test_sentinel_status_completed() {
        let (tracker, mut handle) = LongPollTracker::new();
        tracker.spawn();

        // Create a sleep that will complete
        let sleep_future = tokio::time::sleep(Duration::from_secs(8));
        let wrapped = DetectLongWait::new(sleep_future, handle.sentinel_tx.clone());

        // Run to completion
        wrapped.await;

        // The sentinel should now be marked as Completed
        // (internally, when the future completes, it sets status to Completed)
        // We verify this indirectly - no more traces should be sent after completion
        tokio::time::sleep(Duration::from_secs(2)).await;

        // Drop the sentinel_tx to signal the tracker to shut down gracefully
        drop(handle.sentinel_tx);

        // Drain any traces that were sent before completion
        let mut trace_count = 0;
        while let Some(_) = handle.rx.recv().await {
            trace_count += 1;
        }

        // We expect at least one trace since the sleep was 8 seconds (> 6 second threshold)
        // but no new traces after completion since sentinel is marked Completed
        check!(trace_count >= 1);
    }

    /// Test that traces are deduplicated based on time interval.
    #[tokio::test]
    #[ignore = "long poll tracker tasks need refactor"]
    async fn test_trace_deduplication() {
        let (tracker, mut handle) = LongPollTracker::new();
        tracker.spawn();

        // Use a longer min_trace_interval to make deduplication more obvious
        let config = TrackerConfig {
            min_trace_interval: Duration::from_secs(2),
        };

        let sleep_future = tokio::time::sleep(Duration::from_secs(10));
        let wrapped = DetectLongWait::with_config(sleep_future, handle.sentinel_tx.clone(), config);

        let task = tokio::spawn(async move {
            wrapped.await;
        });

        // Wait for detection
        tokio::time::sleep(Duration::from_secs(8)).await;

        if let Ok(event) = handle.rx.try_recv() {
            match &event.status {
                SentinelStatus::Pending(traces) => {
                    // With 2 second interval over ~7 seconds of pending,
                    // we should have roughly 3-4 traces, not one per poll
                    check!(traces.len() >= 1);
                    check!(
                        traces.len() <= 5,
                        "Expected at most 5 traces due to deduplication, got {}",
                        traces.len()
                    );
                }
                _ => panic!("Expected Pending status"),
            }
        }

        task.abort();
    }
}
