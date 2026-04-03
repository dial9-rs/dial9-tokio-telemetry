use super::shared_state::{PARKED_SCHED_WAIT, SharedState};
use crate::telemetry::events::{RawEvent, SchedStat};
use crate::telemetry::format::WorkerId;
use crate::telemetry::task_metadata::TaskId;
use std::cell::Cell;
use std::collections::HashMap;
use std::sync::OnceLock;
use std::sync::RwLock;
use tokio::runtime::RuntimeMetrics;

/// Per-runtime state captured at hook registration time.
///
/// All tokio-specific concepts live here rather than in `SharedState`.
/// Each `RuntimeContext` belongs to exactly one tokio runtime.
pub(crate) struct RuntimeContext {
    /// Optional human-readable name, set via `with_runtime_name`.
    pub runtime_name: Option<String>,
    /// Set once after `builder.build()`. Contains the runtime metrics and the
    /// pre-reserved base worker ID for this runtime (`global_id = base + local_index`).
    pub metrics_and_base: OnceLock<(RuntimeMetrics, u64)>,
    /// Maps worker_index → global worker_id within this runtime.
    /// Populated lazily the first time each worker thread resolves its identity.
    pub worker_ids: RwLock<HashMap<usize, u64>>,
}

thread_local! {
    /// Global worker ID for this thread, set on every `resolve_worker` call.
    /// Read by `current_worker_id()` for wake events.
    static GLOBAL_WORKER_ID: Cell<Option<u64>> = const { Cell::new(None) };
    /// Whether we've registered this thread's worker_id mapping.
    static WORKER_REGISTERED: Cell<bool> = const { Cell::new(false) };
    /// Whether we've registered this thread's OS tid for CPU profiling.
    #[cfg(feature = "cpu-profiling")]
    static TID_REGISTERED: Cell<bool> = const { Cell::new(false) };
}

impl RuntimeContext {
    pub(crate) fn new(runtime_name: Option<String>) -> Self {
        Self {
            runtime_name,
            metrics_and_base: OnceLock::new(),
            worker_ids: RwLock::new(HashMap::new()),
        }
    }

    /// Build segment metadata entries for this runtime, e.g. `("runtime.main", "0,1,2,3")`.
    /// Returns `None` if unnamed or no workers resolved yet.
    pub(crate) fn metadata_entry(&self) -> Option<(String, String)> {
        let name = self.runtime_name.as_deref()?;
        let ids = self.worker_ids.read().unwrap();
        if ids.is_empty() {
            return None;
        }
        let mut sorted: Vec<u64> = ids.values().copied().collect();
        sorted.sort_unstable();
        let csv = sorted
            .iter()
            .map(|id| id.to_string())
            .collect::<Vec<_>>()
            .join(",");
        Some((format!("runtime.{name}"), csv))
    }

    /// Sum of global queue depth for this runtime (0 if metrics not yet set).
    pub(crate) fn global_queue_depth(&self) -> usize {
        self.metrics_and_base
            .get()
            .map(|(m, _)| m.global_queue_depth())
            .unwrap_or(0)
    }

    /// Local queue depth for a worker in this runtime.
    fn local_queue_depth(&self, worker_index: usize) -> usize {
        self.metrics_and_base
            .get()
            .map(|(m, _)| m.worker_local_queue_depth(worker_index))
            .unwrap_or(0)
    }

    /// Resolve the current thread's global worker ID using `tokio::runtime::worker_index()`.
    fn resolve_worker(&self, shared: &SharedState) -> Option<(WorkerId, usize)> {
        let local_index = tokio::runtime::worker_index()?;
        let (_, base) = self.metrics_and_base.get()?;
        let global_id = base + local_index as u64;

        // Always update TLS so current_worker_id() returns the global ID.
        GLOBAL_WORKER_ID.with(|cell| cell.set(Some(global_id)));

        register_worker_if_needed(self, local_index, global_id);
        #[cfg(feature = "cpu-profiling")]
        register_tid_if_needed(global_id, shared);
        #[cfg(not(feature = "cpu-profiling"))]
        let _ = shared;

        Some((WorkerId::from(global_id as usize), local_index))
    }
}

/// Record worker_index → global_id in the context's map (once per thread).
fn register_worker_if_needed(ctx: &RuntimeContext, local_index: usize, global_id: u64) {
    WORKER_REGISTERED.with(|cell| {
        if !cell.get() {
            ctx.worker_ids
                .write()
                .unwrap()
                .insert(local_index, global_id);
            cell.set(true);
        }
    });
}

/// Register the current thread's OS tid for CPU profiling (once per thread).
#[cfg(feature = "cpu-profiling")]
fn register_tid_if_needed(global_id: u64, shared: &SharedState) {
    TID_REGISTERED.with(|cell| {
        if !cell.get() {
            let os_tid = crate::telemetry::events::current_tid();
            shared.thread_roles.lock().unwrap().insert(
                os_tid,
                crate::telemetry::events::ThreadRole::Worker(global_id as usize),
            );
            cell.set(true);
        }
    });
}

/// Get the current thread's global worker ID (255 = unknown).
/// Used by the `Traced` waker to record which worker issued the wake.
pub(crate) fn current_worker_id() -> u8 {
    GLOBAL_WORKER_ID.with(|cell| {
        cell.get()
            // TODO: cleanly handle more than 255 global workers
            .map(|id| if id <= 254 { id as u8 } else { 255 })
            .unwrap_or(255)
    })
}

// ── Event construction helpers ───────────────────────────────────────────────

pub(super) fn make_poll_start(
    ctx: &RuntimeContext,
    shared: &SharedState,
    location: &'static std::panic::Location<'static>,
    task_id: TaskId,
) -> RawEvent {
    let resolved = ctx.resolve_worker(shared);
    let worker_local_queue_depth = resolved
        .map(|(_, idx)| ctx.local_queue_depth(idx))
        .unwrap_or(0);
    RawEvent::PollStart {
        timestamp_nanos: crate::telemetry::events::clock_monotonic_ns(),
        worker_id: resolved.map(|(id, _)| id).unwrap_or(WorkerId::UNKNOWN),
        worker_local_queue_depth,
        task_id,
        location,
    }
}

pub(super) fn make_poll_end(ctx: &RuntimeContext, shared: &SharedState) -> RawEvent {
    let resolved = ctx.resolve_worker(shared);
    RawEvent::PollEnd {
        timestamp_nanos: crate::telemetry::events::clock_monotonic_ns(),
        worker_id: resolved.map(|(id, _)| id).unwrap_or(WorkerId::UNKNOWN),
    }
}

pub(super) fn make_worker_park(ctx: &RuntimeContext, shared: &SharedState) -> RawEvent {
    let resolved = ctx.resolve_worker(shared);
    let worker_local_queue_depth = resolved
        .map(|(_, idx)| ctx.local_queue_depth(idx))
        .unwrap_or(0);
    let cpu_time_nanos = crate::telemetry::events::thread_cpu_time_nanos();
    if let Ok(ss) = SchedStat::read_current() {
        PARKED_SCHED_WAIT.with(|c| c.set(ss.wait_time_ns));
    }
    RawEvent::WorkerPark {
        timestamp_nanos: crate::telemetry::events::clock_monotonic_ns(),
        worker_id: resolved.map(|(id, _)| id).unwrap_or(WorkerId::UNKNOWN),
        worker_local_queue_depth,
        cpu_time_nanos,
    }
}

pub(super) fn make_worker_unpark(ctx: &RuntimeContext, shared: &SharedState) -> RawEvent {
    let resolved = ctx.resolve_worker(shared);
    let worker_local_queue_depth = resolved
        .map(|(_, idx)| ctx.local_queue_depth(idx))
        .unwrap_or(0);
    let cpu_time_nanos = crate::telemetry::events::thread_cpu_time_nanos();
    let sched_wait_delta_nanos = if let Ok(ss) = SchedStat::read_current() {
        let prev = PARKED_SCHED_WAIT.with(|c| c.get());
        ss.wait_time_ns.saturating_sub(prev)
    } else {
        0
    };
    RawEvent::WorkerUnpark {
        timestamp_nanos: crate::telemetry::events::clock_monotonic_ns(),
        worker_id: resolved.map(|(id, _)| id).unwrap_or(WorkerId::UNKNOWN),
        worker_local_queue_depth,
        cpu_time_nanos,
        sched_wait_delta_nanos,
    }
}
