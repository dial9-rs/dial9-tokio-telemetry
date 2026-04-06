use crate::telemetry::buffer;
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
        buffer::record_event(event, &self.collector);
    }
}
