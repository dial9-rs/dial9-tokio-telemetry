//! Core telemetry module.
//!
//! All public types are re-exported here — use `dial9_tokio_telemetry::telemetry::*`
//! rather than reaching into sub-modules.

#[cfg(feature = "analysis")]
pub(crate) mod analysis;
pub(crate) mod buffer;
pub(crate) mod collector;
#[cfg(feature = "cpu-profiling")]
pub(crate) mod cpu_profile;
pub(crate) mod events;
pub(crate) mod format;
pub(crate) mod recorder;
pub(crate) mod task_metadata;
pub(crate) mod writer;

#[cfg(feature = "cpu-profiling")]
pub use cpu_profile::CpuProfilingConfig;
#[cfg(feature = "cpu-profiling")]
pub use cpu_profile::SchedEventConfig;
pub use dial9_trace_format::InternedString;
pub use events::{
    CpuSampleData, CpuSampleSource, RawEvent, SchedStat, TelemetryEvent, clock_monotonic_ns,
};
pub use format::{PollEndEvent, WorkerId};
pub use recorder::{
    HasTracePath, NoTracePath, TelemetryGuard, TelemetryHandle, TracedRuntime, TracedRuntimeBuilder,
};
pub use task_metadata::{TaskId, UNKNOWN_TASK_ID};
pub use writer::{NullWriter, RotatingWriter, TraceWriter};
