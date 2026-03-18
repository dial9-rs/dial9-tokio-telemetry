#[cfg(feature = "cpu-profiling")]
use crate::telemetry::events::{CpuSampleData, RawEvent};

/// CPU-profiling interning state: tracks which defs (thread names) have been
/// emitted in the current file so they can be re-emitted after rotation.
///
/// This struct is intentionally *not* aware of writers, profilers, or flush
/// orchestration; that lives in [`super::event_writer::EventWriter`].
#[cfg(feature = "cpu-profiling")]
pub(super) struct CpuFlushState;

#[cfg(feature = "cpu-profiling")]
impl CpuFlushState {
    pub(super) fn new() -> Self {
        Self
    }

    /// Called on file rotation.
    pub(super) fn on_rotate(&mut self) {}

    /// Wrap a CPU sample as a `RawEvent`.
    pub(super) fn resolve_cpu_event_symbols(&mut self, data: &CpuSampleData) -> Vec<RawEvent> {
        vec![RawEvent::CpuSample(Box::new(data.clone()))]
    }
}
