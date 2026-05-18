//! Source trait for abstracting flush-thread data sources.

use crate::primitives::sync::Arc;
use crate::primitives::sync::atomic::AtomicU64;
use crate::telemetry::collector::CentralCollector;
use crate::telemetry::events::ThreadRole;
use std::collections::HashMap;

/// Context passed to [`Source::flush`] containing shared state needed for draining.
pub(crate) struct FlushContext<'a> {
    pub collector: &'a Arc<CentralCollector>,
    pub drain_epoch: &'a AtomicU64,
    pub thread_roles: &'a HashMap<u32, ThreadRole>,
}

/// A data source that the flush thread drains into the central collector.
///
/// Implementors (e.g. `CpuProfiler`, `SchedProfiler`) provide a `flush` method
/// that drains pending data and records it via `record_encodable_event`.
pub(crate) trait Source: Send {
    /// Drain pending data into the dial9 trace. Called once per flush cycle
    /// from the flush thread.
    fn flush(&mut self, ctx: &FlushContext<'_>);

    /// Diagnostic name (e.g. "cpu_profile", "sched").
    fn name(&self) -> &'static str;

    /// Called when a worker thread starts. Used by per-thread sources like SchedProfiler
    /// to start tracking the current thread. Returns an error if tracking fails.
    fn on_worker_thread_start(&mut self) -> std::io::Result<()> {
        Ok(())
    }

    /// Called when a thread stops. Used by per-thread sources like SchedProfiler
    /// to stop tracking the current thread.
    fn on_thread_stop(&mut self) {}
}
