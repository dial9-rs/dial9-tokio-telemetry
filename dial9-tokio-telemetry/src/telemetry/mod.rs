//! Core telemetry module.
//!
//! All public types are re-exported here — use `dial9_tokio_telemetry::telemetry::*`
//! rather than reaching into sub-modules.

#[cfg(feature = "analysis")]
pub(crate) mod analysis;
pub(crate) mod buffer;
pub(crate) mod collector;
pub use collector::Batch;
#[cfg(feature = "cpu-profiling")]
pub mod cpu_profile;
pub(crate) mod events;
pub(crate) mod format;
pub(crate) mod recorder;
pub(crate) mod task_metadata;
pub(crate) mod writer;

pub use events::{CpuSampleSource, TelemetryEvent, clock_monotonic_ns};
pub use format::{
    PollEndEvent, PollStartEvent, TaskSpawnEvent, WakeEventEvent, WorkerId, WorkerParkEvent,
    WorkerUnparkEvent,
};
pub use recorder::{
    HasTracePath, NoTracePath, TelemetryGuard, TelemetryHandle, TracedRuntime, TracedRuntimeBuilder,
};
pub use task_metadata::{TaskId, UNKNOWN_TASK_ID};
pub use writer::{NullWriter, RotatingWriter, TraceWriter};
