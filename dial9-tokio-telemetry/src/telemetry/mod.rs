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

pub use buffer::{Encodable, ThreadLocalEncoder};
pub use events::{CpuSampleSource, TelemetryEvent, clock_monotonic_ns};
pub use format::{
    PollEndEvent, PollStartEvent, TaskSpawnEvent, WakeEventEvent, WorkerId, WorkerParkEvent,
    WorkerUnparkEvent,
};
pub use recorder::{
    HasTracePath, NoTracePath, RuntimeTelemetryHandle, TelemetryCore, TelemetryGuard,
    TelemetryHandle, TraceRuntimeCoreBuilder, TracedRuntime, TracedRuntimeBuilder,
};
pub use task_metadata::{TaskId, UNKNOWN_TASK_ID};
pub use writer::{NullWriter, RotatingWriter, TraceWriter};

/// Record a custom event into the trace.
///
/// Events are encoded into a thread-local buffer and flushed to disk by the
/// background flush thread. This function is very cheap (~100–200 ns) and
/// safe to call on hot paths.
///
/// Any type implementing [`dial9_trace_format::TraceEvent`] (typically via
/// `#[derive(TraceEvent)]`) automatically implements [`Encodable`] and can
/// be passed directly. For events that need string interning, implement
/// [`Encodable`] manually.
///
/// Does nothing if telemetry is disabled on the handle.
///
/// # Example
///
/// ```ignore
/// use dial9_trace_format::TraceEvent;
/// use dial9_tokio_telemetry::telemetry::{record_event, clock_monotonic_ns};
///
/// #[derive(TraceEvent)]
/// struct HttpRequest {
///     #[traceevent(timestamp)]
///     timestamp_ns: u64,
///     status_code: u32,
/// }
///
/// record_event(
///     HttpRequest { timestamp_ns: clock_monotonic_ns(), status_code: 200 },
///     &handle,
/// );
/// ```
pub fn record_event(event: impl Encodable, handle: &TelemetryHandle) {
    handle.record_encodable_event(&event);
}
