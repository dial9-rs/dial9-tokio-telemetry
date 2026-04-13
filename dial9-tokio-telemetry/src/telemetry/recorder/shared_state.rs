use crate::metrics::TlDrainStats;
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
    /// Whether task spawn/terminate tracking is enabled for this session.
    pub(crate) task_tracking_enabled: bool,
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
    /// cycle for queue sampling and metadata generation. `build_and_attach_to_telemetry`
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
    pub(super) fn new(start_time_ns: u64, task_tracking_enabled: bool) -> Self {
        Self {
            enabled: AtomicBool::new(false),
            collector: Arc::new(CentralCollector::new()),
            start_time_ns,
            task_tracking_enabled,
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
    ///
    /// Returns per-cycle counters so the flush thread can emit metrics.
    pub(crate) fn drain_all_tl_buffers(&self) -> TlDrainStats {
        let mut stats = TlDrainStats::default();
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
                stats.buffers_skipped_busy += 1;
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
                stats.buffers_locked += 1;
                if buf.has_pending_events() {
                    let batch = buf.flush();
                    stats.events_flushed += batch.event_count();
                    stats.buffers_flushed += 1;
                    self.collector.accept_flush(batch);
                }
                // Stamp so we skip this buffer next cycle if it stays idle.
                handle.flush_epoch.store(epoch);
            }
        }

        // Prune dead handles (Weak refs to threads that have exited).
        let mut guard = self.tl_buffers.lock().unwrap();
        let before = guard.len();
        guard.retain(|h| h.buffer.strong_count() > 0);
        stats.dead_pruned = (before - guard.len()) as u64;

        stats
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
        let ss = SharedState::new(0, false);
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

    /// Intrusive-drain path with a *live* worker thread. Unlike
    /// `drain_all_tl_buffers_from_another_thread`, which joins the worker
    /// before draining (so events reach the collector via the TLB `Drop`
    /// impl, not via the intrusive path), here the worker is parked on a
    /// channel while the main thread bumps+drains, proving that
    /// `drain_all_tl_buffers` upgrades the live `Weak`, locks the mutex
    /// cross-thread, and flushes the pending event.
    #[test]
    fn drain_flushes_live_worker_buffer() {
        let ss = Arc::new(enabled_shared_state());
        let ss2 = ss.clone();
        let (release_tx, release_rx) = std::sync::mpsc::channel::<()>();
        let (ready_tx, ready_rx) = std::sync::mpsc::channel::<()>();

        let worker = std::thread::spawn(move || {
            // drain_epoch is 0, so no self-flush happens — the event
            // stays in the buffer.
            ss2.record_event(poll_end_event());
            ready_tx.send(()).unwrap();
            // Park until main thread has drained. The TLB `Drop` impl must
            // not run before the intrusive drain, otherwise we're not
            // testing the intrusive path.
            release_rx.recv().unwrap();
        });

        ready_rx.recv().unwrap();
        // Worker is parked with one event in its TLB and a live handle.
        // Nothing in the collector yet — no self-flush was triggered.
        assert!(ss.collector.next().is_none());

        ss.bump_drain_epoch();
        ss.drain_all_tl_buffers();

        let batch = ss
            .collector
            .next()
            .expect("intrusive drain should have flushed the live worker's event");
        assert_eq!(batch.event_count, 1);

        release_tx.send(()).unwrap();
        worker.join().unwrap();
    }

    // Concurrent-stress proptest: the core invariant of the TL buffer
    // drain feature is that no events are lost and none are duplicated,
    // regardless of how `record_event`, `bump_drain_epoch`, and
    // `drain_all_tl_buffers` interleave across threads. Spawn N writer
    // threads, each recording M events, while a drainer thread
    // concurrently bumps+drains. After joining, a final bump+drain should
    // leave exactly N*M events in the collector.
    proptest::proptest! {
        #![proptest_config(proptest::prelude::ProptestConfig::with_cases(32))]

        #[test]
        fn concurrent_record_and_drain_preserves_event_count(
            num_threads in 1usize..=6,
            events_per_thread in 1u64..=200,
            drain_ticks in 0usize..=10,
        ) {
            let ss = Arc::new(enabled_shared_state());
            let start = Arc::new(std::sync::Barrier::new(num_threads + 1));
            let stop_drainer = Arc::new(AtomicBool::new(false));

            let writers: Vec<_> = (0..num_threads)
                .map(|_| {
                    let ss = ss.clone();
                    let start = start.clone();
                    std::thread::spawn(move || {
                        start.wait();
                        for _ in 0..events_per_thread {
                            ss.record_event(poll_end_event());
                        }
                    })
                })
                .collect();

            let drainer = {
                let ss = ss.clone();
                let stop = stop_drainer.clone();
                std::thread::spawn(move || {
                    let mut ticks = 0;
                    while ticks < drain_ticks && !stop.load(Ordering::Relaxed) {
                        ss.bump_drain_epoch();
                        // Short grace period so any in-flight writer has a
                        // chance to self-flush before the intrusive drain.
                        std::thread::sleep(std::time::Duration::from_micros(50));
                        ss.drain_all_tl_buffers();
                        ticks += 1;
                    }
                })
            };

            start.wait();
            for w in writers {
                w.join().unwrap();
            }
            stop_drainer.store(true, Ordering::Relaxed);
            drainer.join().unwrap();

            // Writer threads have exited, so their TLB `Drop` impls have
            // flushed any remaining events. Do one final bump+drain to
            // prune dead handles (no-op for event capture at this point).
            ss.bump_drain_epoch();
            ss.drain_all_tl_buffers();

            let mut total: u64 = 0;
            while let Some(batch) = ss.collector.next() {
                total += batch.event_count();
            }
            // Sanity: the collector never evicted a batch under these
            // workloads. If it did, the invariant check below would be
            // meaningless.
            proptest::prop_assert_eq!(ss.collector.take_dropped_batches(), 0);
            proptest::prop_assert_eq!(
                total,
                num_threads as u64 * events_per_thread,
                "every recorded event must reach the collector exactly once"
            );
        }
    }
}
