use crate::telemetry::buffer;
use crate::telemetry::collector::CentralCollector;
#[cfg(feature = "cpu-profiling")]
use crate::telemetry::events::ThreadRole;
use crate::telemetry::events::{RawEvent, SchedStat};
use crate::telemetry::format::WorkerId;
use crate::telemetry::task_metadata::TaskId;
use arc_swap::ArcSwap;
use std::cell::Cell;
#[cfg(feature = "cpu-profiling")]
use std::collections::HashMap;
use std::sync::Arc;
#[cfg(feature = "cpu-profiling")]
use std::sync::Mutex;
use std::sync::atomic::{AtomicBool, Ordering};
use tokio::runtime::RuntimeMetrics;

thread_local! {
    /// Cached tokio worker index for this thread. `None` means not yet resolved.
    /// Once resolved, the worker ID is stable for the lifetime of the thread—a thread
    /// won't become a *different* worker, though it may stop being a worker entirely.
    static WORKER_ID: Cell<Option<usize>> = const { Cell::new(None) };
    /// Negative cache: true once we've confirmed this thread is NOT a worker.
    /// Only set after a full scan with metrics available.
    static NOT_A_WORKER: Cell<bool> = const { Cell::new(false) };
    /// Whether we've already registered this thread's tid→worker mapping.
    static TID_EMITTED: Cell<bool> = const { Cell::new(false) };
    /// schedstat wait_time_ns captured at park time, used to compute delta on unpark.
    pub(super) static PARKED_SCHED_WAIT: Cell<u64> = const { Cell::new(0) };
}

/// Resolve the current thread's tokio worker index, caching in TLS.
/// Returns None if the thread is not a tokio worker.
pub(super) fn resolve_worker_id(
    metrics: &ArcSwap<Option<RuntimeMetrics>>,
    shared: Option<&SharedState>,
) -> Option<usize> {
    #[cfg(not(feature = "cpu-profiling"))]
    let _ = shared;
    WORKER_ID.with(|cell| {
        if let Some(id) = cell.get() {
            return Some(id);
        }
        if NOT_A_WORKER.with(|c| c.get()) {
            return None;
        }
        let tid = std::thread::current().id();
        if let Some(ref m) = **metrics.load() {
            for i in 0..m.num_workers() {
                if m.worker_thread_id(i) == Some(tid) {
                    cell.set(Some(i));
                    #[cfg(feature = "cpu-profiling")]
                    if let Some(shared) = shared {
                        TID_EMITTED.with(|emitted| {
                            if !emitted.get() {
                                emitted.set(true);
                                let os_tid = crate::telemetry::events::current_tid();
                                shared
                                    .thread_roles
                                    .lock()
                                    .unwrap()
                                    .insert(os_tid, ThreadRole::Worker(i));
                            }
                        });
                    }
                    return Some(i);
                }
            }
            NOT_A_WORKER.with(|c| c.set(true));
        }
        None
    })
}

/// Get the current worker ID (UNKNOWN if not a worker). Used by Traced waker.
pub(crate) fn current_worker_id(metrics: &ArcSwap<Option<RuntimeMetrics>>) -> u8 {
    match resolve_worker_id(metrics, None) {
        Some(id) if id <= 254 => id as u8,
        _ => 255,
    }
}

/// Shared state accessed lock-free by callbacks on the hot path.
pub(crate) struct SharedState {
    pub(crate) enabled: AtomicBool,
    pub(crate) collector: Arc<CentralCollector>,
    /// Absolute `CLOCK_MONOTONIC` nanosecond timestamp captured at trace start.
    pub(crate) start_time_ns: u64,
    /// Tokio metrics allow resolving the current worker id from a thread id
    pub(crate) metrics: ArcSwap<Option<RuntimeMetrics>>,
    /// Maps OS tid → thread role so that CPU samples returned from perf can be
    /// attributed to the correct worker or blocking-pool bucket at flush time.
    /// Entries are inserted in `on_thread_start` and removed in `on_thread_stop`.
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
            metrics: ArcSwap::from_pointee(None),
            #[cfg(feature = "cpu-profiling")]
            thread_roles: Mutex::new(HashMap::new()),
            #[cfg(feature = "cpu-profiling")]
            sched_profiler: Mutex::new(None),
        }
    }

    pub(crate) fn timestamp_nanos(&self) -> u64 {
        crate::telemetry::events::clock_monotonic_ns()
    }

    pub(crate) fn create_wake_event(&self, woken_task_id: TaskId) -> RawEvent {
        let waker_task_id = tokio::task::try_id().map(TaskId::from).unwrap_or_default();
        let target_worker = current_worker_id(&self.metrics);
        RawEvent::WakeEvent {
            timestamp_nanos: self.timestamp_nanos(),
            waker_task_id,
            woken_task_id,
            target_worker,
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
        buffer::record_event(event, &self.collector);
    }

    pub(super) fn make_poll_start(
        &self,
        location: &'static std::panic::Location<'static>,
        task_id: TaskId,
    ) -> RawEvent {
        let worker_id = resolve_worker_id(&self.metrics, Some(self));
        let metrics_guard = self.metrics.load();
        let worker_local_queue_depth =
            if let (Some(worker_id), Some(metrics)) = (worker_id, &**metrics_guard) {
                metrics.worker_local_queue_depth(worker_id)
            } else {
                0
            };
        RawEvent::PollStart {
            timestamp_nanos: crate::telemetry::events::clock_monotonic_ns(),
            worker_id: worker_id.map(WorkerId::from).unwrap_or(WorkerId::UNKNOWN),
            worker_local_queue_depth,
            task_id,
            location,
        }
    }

    pub(super) fn make_poll_end(&self) -> RawEvent {
        let worker_id = resolve_worker_id(&self.metrics, Some(self));
        RawEvent::PollEnd {
            timestamp_nanos: crate::telemetry::events::clock_monotonic_ns(),
            worker_id: worker_id.map(WorkerId::from).unwrap_or(WorkerId::UNKNOWN),
        }
    }

    pub(super) fn make_worker_park(&self) -> RawEvent {
        let worker_id = resolve_worker_id(&self.metrics, Some(self));
        let metrics_guard = self.metrics.load();
        let worker_local_queue_depth =
            if let (Some(worker_id), Some(metrics)) = (worker_id, &**metrics_guard) {
                metrics.worker_local_queue_depth(worker_id)
            } else {
                0
            };
        let cpu_time_nanos = crate::telemetry::events::thread_cpu_time_nanos();
        if let Ok(ss) = SchedStat::read_current() {
            PARKED_SCHED_WAIT.with(|c| c.set(ss.wait_time_ns));
        }
        RawEvent::WorkerPark {
            timestamp_nanos: crate::telemetry::events::clock_monotonic_ns(),
            worker_id: worker_id.map(WorkerId::from).unwrap_or(WorkerId::UNKNOWN),
            worker_local_queue_depth,
            cpu_time_nanos,
        }
    }

    pub(super) fn make_worker_unpark(&self) -> RawEvent {
        let worker_id = resolve_worker_id(&self.metrics, Some(self));
        let metrics_guard = self.metrics.load();
        let worker_local_queue_depth =
            if let (Some(worker_id), Some(metrics)) = (worker_id, &**metrics_guard) {
                metrics.worker_local_queue_depth(worker_id)
            } else {
                0
            };
        let cpu_time_nanos = crate::telemetry::events::thread_cpu_time_nanos();
        let sched_wait_delta_nanos = if let Ok(ss) = SchedStat::read_current() {
            let prev = PARKED_SCHED_WAIT.with(|c| c.get());
            ss.wait_time_ns.saturating_sub(prev)
        } else {
            0
        };
        RawEvent::WorkerUnpark {
            timestamp_nanos: crate::telemetry::events::clock_monotonic_ns(),
            worker_id: worker_id.map(WorkerId::from).unwrap_or(WorkerId::UNKNOWN),
            worker_local_queue_depth,
            cpu_time_nanos,
            sched_wait_delta_nanos,
        }
    }
}
