use std::time::Duration;

/// Configuration for task dump capture.
///
/// Task dumps capture async backtraces at yield points, enabling post-hoc
/// analysis of what tasks were waiting on when they were idle.
///
/// Use [`TaskDumpConfig::enabled()`] for defaults, or [`TaskDumpConfig::builder()`]
/// to customize.
#[derive(Debug, Clone, bon::Builder)]
pub struct TaskDumpConfig {
    /// Whether task dumps are enabled. Defaults to `true`.
    #[builder(default = true)]
    enabled: bool,
    /// Minimum idle duration before a task dump is emitted. Defaults to 10ms.
    #[builder(default = Duration::from_millis(10))]
    idle_threshold: Duration,
}

impl TaskDumpConfig {
    /// Create a config with task dumps enabled and default settings.
    pub fn enabled() -> Self {
        Self {
            enabled: true,
            idle_threshold: Duration::from_millis(10),
        }
    }

    /// Whether task dumps are enabled.
    pub fn is_enabled(&self) -> bool {
        self.enabled
    }

    /// The idle threshold duration.
    pub fn idle_threshold(&self) -> Duration {
        self.idle_threshold
    }
}
