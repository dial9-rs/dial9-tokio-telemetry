use crate::telemetry::buffer;
use crate::telemetry::buffer::TlBufferHandle;
use crate::telemetry::collector::CentralCollector;
use crate::telemetry::events::RawEvent;
#[cfg(feature = "cpu-profiling")]
use crate::telemetry::events::ThreadRole;
use crate::telemetry::task_metadata::TaskId;
use std::cell::Cell;
#[cfg(feature = "cpu-profiling")]
use std::collections::HashMap;
use std::sync::Arc;
use std::sync::Mutex;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::time::Duration;

use super::RuntimeContext;

thread_local! {
    /// schedstat wait_time_ns captured at park time, used to compute delta on unpark.
    pub(super) static PARKED_SCHED_WAIT: Cell<u64> = const { Cell::new(0) };
}

/// Runtime-agnostic core recording state.
///
/// No tokio imports. All runtime-specific logic lives in `RuntimeContext`.
pub(crate) struct SharedState {
    pub(crate) enabled: AtomicBool,
    pub(crate) collector: Arc<CentralCollector>,
    /// Absolute `CLOCK_MONOTONIC` nanosecond timestamp captured at trace start.
    pub(crate) start_time_ns: u64,
    /// Global worker ID counter. Each runtime reserves a contiguous block
    /// via `fetch_add(num_workers)` so worker IDs don't collide.
    pub(crate) next_worker_id: AtomicU64,
    /// Epoch counter bumped by the flush thread every ~30s. Thread-local
    /// buffers stamp this value on each self-flush so the flush thread can
    /// skip busy workers when draining.
    pub(crate) drain_epoch: AtomicU64,
    /// Weak handles to all registered thread-local buffers. The flush thread
    /// uses these to intrusively drain idle/silent buffers.
    tl_buffers: Mutex<Vec<TlBufferHandle>>,
    /// All registered `RuntimeContext`s. The flush thread clones this vec each
    /// cycle for queue sampling and metadata generation. `build_with_reuse`
    /// pushes new contexts here so the flush thread picks them up.
    pub(crate) contexts: Mutex<Vec<Arc<RuntimeContext>>>,
    /// Maps OS tid → thread role so that CPU samples returned from perf can be
    /// attributed to the correct worker or blocking-pool bucket at flush time.
    #[cfg(feature = "cpu-profiling")]
    pub(crate) thread_roles: Mutex<HashMap<u32, ThreadRole>>,
    #[cfg(feature = "cpu-profiling")]
    pub(crate) sched_profiler: Mutex<Option<crate::telemetry::cpu_profile::SchedProfiler>>,
}

impl SharedState {
    pub(super) fn new(start_time_ns: u64) -> Self {
        Self {
            enabled: AtomicBool::new(false),
            collector: Arc::new(CentralCollector::new()),
            start_time_ns,
            next_worker_id: AtomicU64::new(0),
            drain_epoch: AtomicU64::new(0),
            tl_buffers: Mutex::new(Vec::new()),
            contexts: Mutex::new(Vec::new()),
            #[cfg(feature = "cpu-profiling")]
            thread_roles: Mutex::new(HashMap::new()),
            #[cfg(feature = "cpu-profiling")]
            sched_profiler: Mutex::new(None),
        }
    }

    fn timestamp_nanos(&self) -> u64 {
        crate::telemetry::events::clock_monotonic_ns()
    }

    /// Create a wake event. Pragmatic exception: calls `tokio::task::try_id()`
    /// because `Traced` is inherently tokio-specific.
    pub(crate) fn create_wake_event(&self, woken_task_id: TaskId, waking_worker: u8) -> RawEvent {
        let waker_task_id = tokio::task::try_id().map(TaskId::from).unwrap_or_default();
        RawEvent::WakeEvent {
            timestamp_nanos: self.timestamp_nanos(),
            waker_task_id,
            woken_task_id,
            target_worker: waking_worker,
        }
    }

    pub(crate) fn record_queue_sample(&self, global_queue_depth: usize) {
        self.record_event(RawEvent::QueueSample {
            timestamp_nanos: self.timestamp_nanos(),
            global_queue_depth,
        });
    }

    pub(crate) fn record_event(&self, event: RawEvent) {
        if !self.enabled.load(Ordering::Relaxed) {
            return;
        }
        if let Some(handle) = buffer::record_event(event, &self.collector, &self.drain_epoch) {
            self.tl_buffers.lock().unwrap().push(handle);
        }
    }

    /// Bump the drain epoch and flush all idle/silent thread-local buffers.
    ///
    /// Buffers whose `FlushEpoch` matches the current epoch are skipped
    /// (the owning thread flushed recently, so locking would just add
    /// contention). Dead `Weak` handles are pruned.
    ///
    /// [`bump_drain_epoch`] is called one flush-loop tick
    /// before calling this method. That gives busy worker threads a ~5 ms
    /// grace period to self-flush on their next `record_event`, so the
    /// intrusive drain only needs to lock truly idle/silent buffers.
    pub(crate) fn drain_all_tl_buffers(&self) {
        let epoch = self.drain_epoch.load(Ordering::Relaxed);

        let handles: Vec<TlBufferHandle> = {
            let guard = self.tl_buffers.lock().unwrap();
            guard
                .iter()
                .map(|h| TlBufferHandle {
                    buffer: h.buffer.clone(),
                    flush_epoch: h.flush_epoch.clone(),
                })
                .collect()
        };

        for handle in &handles {
            // Skip buffers that self-flushed during the current epoch.
            if handle.flush_epoch.load() >= epoch {
                continue;
            }
            if let Some(arc) = handle.buffer.upgrade() {
                let mut buf = match arc.lock() {
                    Ok(guard) => guard,
                    // Buffer is poisoned (encoder panic); skip rather than
                    // flushing potentially corrupt data.
                    Err(_) => {
                        crate::rate_limit::rate_limited!(Duration::from_secs(60), {
                            tracing::error!(
                                "dial9: thread-local buffer mutex poisoned in drain_all_tl_buffers; skipping flush"
                            );
                        });
                        continue;
                    }
                };
                if buf.has_pending_events() {
                    self.collector.accept_flush(buf.flush());
                }
            }
        }

        // Prune dead handles (Weak refs to threads that have exited).
        self.tl_buffers
            .lock()
            .unwrap()
            .retain(|h| h.buffer.strong_count() > 0);
    }

    /// Advance the global drain epoch so that busy worker threads
    /// self-flush on their next `record_event` call. Call this one
    /// flush-loop tick (~5 ms) before [`drain_all_tl_buffers`] to give
    /// workers a grace period, minimising contention on the intrusive
    /// drain path.
    pub(crate) fn bump_drain_epoch(&self) {
        self.drain_epoch.fetch_add(1, Ordering::Relaxed);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::telemetry::format::WorkerId;

    fn poll_end_event() -> RawEvent {
        RawEvent::PollEnd {
            timestamp_nanos: 1000,
            worker_id: WorkerId::from(0usize),
        }
    }

    /// Helper: create a SharedState with recording enabled.
    fn enabled_shared_state() -> SharedState {
        let ss = SharedState::new(0);
        ss.enabled.store(true, Ordering::Relaxed);
        ss
    }

    #[test]
    fn record_event_registers_tl_buffer_handle() {
        let ss = enabled_shared_state();
        // First event on this thread should register a handle.
        ss.record_event(poll_end_event());
        let handles = ss.tl_buffers.lock().unwrap();
        assert_eq!(handles.len(), 1);
        assert!(handles[0].buffer.upgrade().is_some());
    }

    #[test]
    fn second_record_event_does_not_re_register() {
        let ss = enabled_shared_state();
        ss.record_event(poll_end_event());
        ss.record_event(poll_end_event());
        let handles = ss.tl_buffers.lock().unwrap();
        assert_eq!(handles.len(), 1);
    }

    #[test]
    fn drain_all_tl_buffers_flushes_idle_buffer() {
        let ss = enabled_shared_state();
        // Write an event (won't self-flush — buffer is 1MB).
        ss.record_event(poll_end_event());
        // Nothing in the collector yet (buffer not full).
        assert!(ss.collector.next().is_none());
        // Bump epoch so the idle buffer (epoch 0) is stale, then drain.
        ss.bump_drain_epoch();
        ss.drain_all_tl_buffers();
        let batch = ss.collector.next().expect("expected a batch after drain");
        assert!(batch.event_count > 0);
    }

    #[test]
    fn drain_all_tl_buffers_from_another_thread() {
        let ss = Arc::new(enabled_shared_state());
        let ss2 = ss.clone();
        // Write events from a spawned thread.
        let handle = std::thread::spawn(move || {
            ss2.record_event(poll_end_event());
            ss2.record_event(poll_end_event());
        });
        handle.join().unwrap();
        // Bump epoch so the buffer is stale, then drain from the main thread.
        ss.bump_drain_epoch();
        ss.drain_all_tl_buffers();
        let batch = ss.collector.next().expect("expected a batch after drain");
        assert_eq!(batch.event_count, 2);
    }

    #[test]
    fn drain_skips_busy_buffer() {
        let ss = enabled_shared_state();
        ss.record_event(poll_end_event());
        // Bump epoch to 1 (simulates the tick before the drain).
        ss.bump_drain_epoch();
        // Simulate a self-flush by stamping the current epoch.
        {
            let handles = ss.tl_buffers.lock().unwrap();
            handles[0].flush_epoch.store(1);
        }
        ss.drain_all_tl_buffers();
        // Buffer should NOT have been flushed — collector is empty.
        assert!(ss.collector.next().is_none());
    }

    #[test]
    fn drain_prunes_dead_handles() {
        let ss = Arc::new(enabled_shared_state());
        let ss2 = ss.clone();
        let handle = std::thread::spawn(move || {
            ss2.record_event(poll_end_event());
        });
        handle.join().unwrap();
        // Thread exited — its Arc<Mutex<TLB>> was dropped, Weak is dead.
        // But the TLB's Drop impl flushed remaining events, so the handle
        // is dead. Drain should prune it.
        ss.drain_all_tl_buffers();
        let handles = ss.tl_buffers.lock().unwrap();
        assert_eq!(handles.len(), 0, "dead handle should have been pruned");
    }
}
