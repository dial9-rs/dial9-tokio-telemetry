//! Configuration for task dump capture.
//!
//! Task dumps capture async backtraces at yield points for tasks that have
//! been idle longer than the configured threshold. Use [`TaskDumpConfig`] with
//! [`TracedRuntimeBuilder::with_task_dumps`](crate::telemetry::TracedRuntimeBuilder::with_task_dumps)
//! or [`TelemetryCoreBuilder::task_dump_config`](crate::telemetry::TelemetryCoreBuilder::task_dump_config).
//!
//! Requires the `taskdump` crate feature. With that feature off, this module
//! is still compiled so the configuration API surface stays the same, but no
//! dumps are captured.

use std::time::Duration;

/// Default idle threshold after which a task dump is emitted.
const DEFAULT_IDLE_THRESHOLD: Duration = Duration::from_millis(10);

/// Configuration for task dump capture.
#[derive(Debug, Clone, bon::Builder)]
pub struct TaskDumpConfig {
    /// Minimum time a task must have been idle between polls before a task
    /// dump is emitted on the next poll. Defaults to 10ms.
    #[builder(default = DEFAULT_IDLE_THRESHOLD)]
    idle_threshold: Duration,
}

impl Default for TaskDumpConfig {
    fn default() -> Self {
        Self::builder().build()
    }
}

impl TaskDumpConfig {
    /// Minimum idle duration between polls before a dump is emitted.
    pub fn idle_threshold(&self) -> Duration {
        self.idle_threshold
    }
}
