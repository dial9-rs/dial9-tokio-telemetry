use crate::telemetry::buffer;
use crate::telemetry::collector::CentralCollector;
#[cfg(feature = "cpu-profiling")]
use crate::telemetry::events::ThreadRole;
use crate::telemetry::events::{RawEvent, SchedStat};
use crate::telemetry::format::WorkerId;
use crate::telemetry::task_metadata::TaskId;
use arc_swap::ArcSwap;
use std::cell::Cell;
use std::collections::HashMap;
use std::sync::Arc;
#[cfg(feature = "cpu-profiling")]
use std::sync::Mutex;
use std::sync::RwLock;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use tokio::runtime::RuntimeMetrics;

/// Result of resolving the current thread's worker identity.
#[derive(Debug, Clone, Copy)]
pub(super) struct ResolvedWorker {
    /// Global WorkerId (sequential integer from the global counter).
    pub worker_id: WorkerId,
    /// Runtime index this worker belongs to.
    pub runtime_index: u64,
    /// Worker index within its runtime (for RuntimeMetrics queries).
    pub worker_index: usize,
}

thread_local! {
    /// Cached tokio worker index for this thread. `None` means not yet resolved.
    /// Once resolved, the worker ID is stable for the lifetime of the thread—a thread
    /// won't become a *different* worker, though it may stop being a worker entirely.
    static WORKER_ID: Cell<Option<ResolvedWorker>> = const { Cell::new(None) };
    /// Negative cache: true once we've confirmed this thread is NOT a worker.
    /// Only set after a full scan with metrics available.
    static NOT_A_WORKER: Cell<bool> = const { Cell::new(false) };
    /// Whether we've already registered this thread's tid→worker mapping.
    static TID_EMITTED: Cell<bool> = const { Cell::new(false) };
    /// schedstat wait_time_ns captured at park time, used to compute delta on unpark.
    pub(super) static PARKED_SCHED_WAIT: Cell<u64> = const { Cell::new(0) };
}

/// Resolve the current thread's tokio worker identity, caching in TLS.
/// Scans all registered runtimes' metrics to find which runtime and worker
/// index this thread belongs to. Returns None if the thread is not a tokio worker.
pub(super) fn resolve_worker_id(
    metrics: &ArcSwap<HashMap<u64, RuntimeMetrics>>,
    shared: Option<&SharedState>,
) -> Option<ResolvedWorker> {
    WORKER_ID.with(|cell| {
        if let Some(resolved) = cell.get() {
            return Some(resolved);
        }
        if NOT_A_WORKER.with(|c| c.get()) {
            return None;
        }
        let tid = std::thread::current().id();
        let runtimes = metrics.load();
        for (runtime_index, m) in runtimes.iter() {
            for i in 0..m.num_workers() {
                if m.worker_thread_id(i) == Some(tid) {
                    // Allocate a globally unique worker ID from the atomic counter.
                    let global_id = shared
                        .map(|s| s.next_worker_id.fetch_add(1, Ordering::Relaxed))
                        .unwrap_or(0);
                    let resolved = ResolvedWorker {
                        worker_id: WorkerId::from(global_id as usize),
                        runtime_index: *runtime_index,
                        worker_index: i,
                    };
                    cell.set(Some(resolved));
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
                    return Some(resolved);
                }
            }
        }
        // Only set negative cache if at least one runtime has metrics registered.
        if !runtimes.is_empty() {
            NOT_A_WORKER.with(|c| c.set(true));
        }
        None
    })
}

/// Get the current worker ID (UNKNOWN if not a worker). Used by Traced waker.
pub(crate) fn current_worker_id(metrics: &ArcSwap<HashMap<u64, RuntimeMetrics>>) -> u8 {
    match resolve_worker_id(metrics, None) {
        Some(resolved) if resolved.worker_index <= 254 => resolved.worker_index as u8,
        _ => 255,
    }
}

/// Shared state accessed lock-free by callbacks on the hot path.
pub(crate) struct SharedState {
    pub(crate) enabled: AtomicBool,
    pub(crate) collector: Arc<CentralCollector>,
    /// Absolute `CLOCK_MONOTONIC` nanosecond timestamp captured at trace start.
    pub(crate) start_time_ns: u64,
    /// Tokio metrics for all attached runtimes. Each entry is (runtime_index, metrics).
    /// Used to resolve the current thread's worker identity.
    pub(crate) metrics: ArcSwap<HashMap<u64, RuntimeMetrics>>,
    /// Counter for allocating unique runtime indices.
    pub(crate) next_runtime_index: AtomicU64,
    /// Global worker ID counter. Each worker gets a unique ID on first resolution.
    pub(crate) next_worker_id: AtomicU64,
    /// Maps runtime_index → human-readable runtime name (for segment metadata).
    pub(crate) runtime_names: RwLock<HashMap<u64, String>>,
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
            metrics: ArcSwap::from_pointee(HashMap::new()),
            next_runtime_index: AtomicU64::new(0),
            next_worker_id: AtomicU64::new(0),
            runtime_names: RwLock::new(HashMap::new()),
            #[cfg(feature = "cpu-profiling")]
            thread_roles: Mutex::new(HashMap::new()),
            #[cfg(feature = "cpu-profiling")]
            sched_profiler: Mutex::new(None),
        }
    }

    /// Allocate the next unique runtime index.
    pub(crate) fn alloc_runtime_index(&self) -> u64 {
        self.next_runtime_index.fetch_add(1, Ordering::Relaxed)
    }

    /// Register a runtime's metrics under the given index.
    pub(crate) fn register_runtime_metrics(&self, runtime_index: u64, new_metrics: RuntimeMetrics) {
        let current = self.metrics.load();
        let mut updated = (**current).clone();
        updated.insert(runtime_index, new_metrics);
        self.metrics.store(Arc::new(updated));
    }

    /// Register a human-readable name for a runtime index.
    pub(crate) fn register_runtime_name(&self, runtime_index: u64, name: String) {
        let mut names = self.runtime_names.write().unwrap();
        names.insert(runtime_index, name);
    }

    /// Get all runtime name mappings (for segment metadata).
    pub(crate) fn runtime_name_entries(&self) -> Vec<(String, String)> {
        let names = self.runtime_names.read().unwrap();
        names
            .iter()
            .map(|(idx, name)| (format!("runtime.{idx}.name"), name.clone()))
            .collect()
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

    /// Look up the local queue depth for a resolved worker.
    fn local_queue_depth(&self, resolved: Option<ResolvedWorker>) -> usize {
        let Some(r) = resolved else { return 0 };
        let runtimes = self.metrics.load();
        runtimes
            .get(&r.runtime_index)
            .map(|m| m.worker_local_queue_depth(r.worker_index))
            .unwrap_or(0)
    }

    pub(super) fn make_poll_start(
        &self,
        location: &'static std::panic::Location<'static>,
        task_id: TaskId,
    ) -> RawEvent {
        let resolved = resolve_worker_id(&self.metrics, Some(self));
        let worker_local_queue_depth = self.local_queue_depth(resolved);
        RawEvent::PollStart {
            timestamp_nanos: crate::telemetry::events::clock_monotonic_ns(),
            worker_id: resolved.map(|r| r.worker_id).unwrap_or(WorkerId::UNKNOWN),
            worker_local_queue_depth,
            task_id,
            location,
        }
    }

    pub(super) fn make_poll_end(&self) -> RawEvent {
        let resolved = resolve_worker_id(&self.metrics, Some(self));
        RawEvent::PollEnd {
            timestamp_nanos: crate::telemetry::events::clock_monotonic_ns(),
            worker_id: resolved.map(|r| r.worker_id).unwrap_or(WorkerId::UNKNOWN),
        }
    }

    pub(super) fn make_worker_park(&self) -> RawEvent {
        let resolved = resolve_worker_id(&self.metrics, Some(self));
        let worker_local_queue_depth = self.local_queue_depth(resolved);
        let cpu_time_nanos = crate::telemetry::events::thread_cpu_time_nanos();
        if let Ok(ss) = SchedStat::read_current() {
            PARKED_SCHED_WAIT.with(|c| c.set(ss.wait_time_ns));
        }
        RawEvent::WorkerPark {
            timestamp_nanos: crate::telemetry::events::clock_monotonic_ns(),
            worker_id: resolved.map(|r| r.worker_id).unwrap_or(WorkerId::UNKNOWN),
            worker_local_queue_depth,
            cpu_time_nanos,
        }
    }

    pub(super) fn make_worker_unpark(&self) -> RawEvent {
        let resolved = resolve_worker_id(&self.metrics, Some(self));
        let worker_local_queue_depth = self.local_queue_depth(resolved);
        let cpu_time_nanos = crate::telemetry::events::thread_cpu_time_nanos();
        let sched_wait_delta_nanos = if let Ok(ss) = SchedStat::read_current() {
            let prev = PARKED_SCHED_WAIT.with(|c| c.get());
            ss.wait_time_ns.saturating_sub(prev)
        } else {
            0
        };
        RawEvent::WorkerUnpark {
            timestamp_nanos: crate::telemetry::events::clock_monotonic_ns(),
            worker_id: resolved.map(|r| r.worker_id).unwrap_or(WorkerId::UNKNOWN),
            worker_local_queue_depth,
            cpu_time_nanos,
            sched_wait_delta_nanos,
        }
    }
}
