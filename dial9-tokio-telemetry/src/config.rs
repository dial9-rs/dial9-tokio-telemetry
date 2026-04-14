//! Unified configuration for the `#[dial9_tokio_telemetry::main]` macro.
//!
//! [`Dial9Config`] bundles writer, runtime, and telemetry settings into a
//! single builder so that the macro can construct everything from one
//! configuration function.

use std::path::PathBuf;
use std::time::Duration;

use crate::telemetry::recorder::{TelemetryGuard, TracedRuntime};
use crate::telemetry::writer::RotatingWriter;

/// Runtime flavor, mirroring [`tokio::runtime::Builder`] constructors.
#[derive(Debug, Clone, Copy)]
pub enum RuntimeFlavor {
    /// Multi-threaded runtime (`Builder::new_multi_thread`).
    MultiThread,
    /// Current-thread runtime (`Builder::new_current_thread`).
    CurrentThread,
}

/// Unified configuration produced by a user-supplied config function and
/// consumed by the `#[main]` macro to build the traced runtime.
///
/// Only the three required writer fields are stored directly. All other
/// settings are `Option`s — when `None`, the underlying builder's default
/// is used, keeping a single source of truth.
#[derive(Debug)]
pub struct Dial9Config {
    // Writer (required)
    base_path: PathBuf,
    max_file_size: u64,
    max_total_size: u64,

    // Writer (optional — defaults come from RotatingWriter)
    rotation_period: Option<Duration>,

    // Tokio runtime (optional — defaults come from tokio)
    flavor: Option<RuntimeFlavor>,
    worker_threads: Option<usize>,

    // TracedRuntime (optional — defaults come from TracedRuntimeBuilder)
    install: Option<bool>,
    task_tracking: Option<bool>,
    runtime_name: Option<String>,
    worker_poll_interval: Option<Duration>,

    // Feature-gated
    #[cfg(feature = "cpu-profiling")]
    cpu_profiling: Option<crate::telemetry::cpu_profile::CpuProfilingConfig>,
    #[cfg(feature = "cpu-profiling")]
    sched_events: Option<crate::telemetry::cpu_profile::SchedEventConfig>,
    #[cfg(feature = "worker-s3")]
    s3_config: Option<crate::background_task::s3::S3Config>,
}

impl Dial9Config {
    /// Create a new builder. `base_path`, `max_file_size`, and
    /// `max_total_size` must be set before calling
    /// [`Dial9ConfigBuilder::build`].
    pub fn builder() -> Dial9ConfigBuilder {
        Dial9ConfigBuilder {
            base_path: None,
            max_file_size: None,
            max_total_size: None,
            rotation_period: None,
            flavor: None,
            worker_threads: None,
            install: None,
            task_tracking: None,
            runtime_name: None,
            worker_poll_interval: None,
            #[cfg(feature = "cpu-profiling")]
            cpu_profiling: None,
            #[cfg(feature = "cpu-profiling")]
            sched_events: None,
            #[cfg(feature = "worker-s3")]
            s3_config: None,
        }
    }

    /// Build the tokio runtime with dial9 telemetry installed and recording
    /// enabled.
    pub fn build(self) -> std::io::Result<(tokio::runtime::Runtime, TelemetryGuard)> {
        let writer = RotatingWriter::builder()
            .base_path(self.base_path.clone())
            .max_file_size(self.max_file_size)
            .max_total_size(self.max_total_size)
            .maybe_rotation_period(self.rotation_period)
            .build()?;

        let mut tokio_builder = match self.flavor.unwrap_or(RuntimeFlavor::MultiThread) {
            RuntimeFlavor::MultiThread => tokio::runtime::Builder::new_multi_thread(),
            RuntimeFlavor::CurrentThread => tokio::runtime::Builder::new_current_thread(),
        };
        if let Some(n) = self.worker_threads {
            tokio_builder.worker_threads(n);
        }
        tokio_builder.enable_all();

        let mut traced = TracedRuntime::builder().with_trace_path(self.base_path);

        if let Some(enabled) = self.install {
            traced = traced.install(enabled);
        }
        if let Some(enabled) = self.task_tracking {
            traced = traced.with_task_tracking(enabled);
        }
        if let Some(name) = self.runtime_name {
            traced = traced.with_runtime_name(name);
        }
        if let Some(interval) = self.worker_poll_interval {
            traced = traced.with_worker_poll_interval(interval);
        }

        #[cfg(feature = "cpu-profiling")]
        if let Some(config) = self.cpu_profiling {
            traced = traced.with_cpu_profiling(config);
        }
        #[cfg(feature = "cpu-profiling")]
        if let Some(config) = self.sched_events {
            traced = traced.with_sched_events(config);
        }
        #[cfg(feature = "worker-s3")]
        if let Some(config) = self.s3_config {
            traced = traced.with_s3_uploader(config);
        }

        traced.build_and_start(tokio_builder, writer)
    }
}

/// Builder for [`Dial9Config`].
///
/// # Required fields
///
/// - [`base_path`](Self::base_path) — trace file path
/// - [`max_file_size`](Self::max_file_size) — per-file rotation threshold
/// - [`max_total_size`](Self::max_total_size) — total disk budget
///
/// All other fields delegate to the underlying builder defaults when unset.
#[derive(Debug)]
pub struct Dial9ConfigBuilder {
    base_path: Option<PathBuf>,
    max_file_size: Option<u64>,
    max_total_size: Option<u64>,
    rotation_period: Option<Duration>,
    flavor: Option<RuntimeFlavor>,
    worker_threads: Option<usize>,
    install: Option<bool>,
    task_tracking: Option<bool>,
    runtime_name: Option<String>,
    worker_poll_interval: Option<Duration>,
    #[cfg(feature = "cpu-profiling")]
    cpu_profiling: Option<crate::telemetry::cpu_profile::CpuProfilingConfig>,
    #[cfg(feature = "cpu-profiling")]
    sched_events: Option<crate::telemetry::cpu_profile::SchedEventConfig>,
    #[cfg(feature = "worker-s3")]
    s3_config: Option<crate::background_task::s3::S3Config>,
}

impl Dial9ConfigBuilder {
    /// Set the trace file path (required).
    pub fn base_path(mut self, path: impl Into<PathBuf>) -> Self {
        self.base_path = Some(path.into());
        self
    }

    /// Set the per-file size limit in bytes (required). The writer rotates
    /// to a new file when this threshold is exceeded.
    pub fn max_file_size(mut self, size: u64) -> Self {
        self.max_file_size = Some(size);
        self
    }

    /// Set the total disk budget in bytes (required). Oldest files are
    /// deleted when total size exceeds this limit.
    pub fn max_total_size(mut self, size: u64) -> Self {
        self.max_total_size = Some(size);
        self
    }

    /// Set the time-based rotation period.
    pub fn rotation_period(mut self, period: Duration) -> Self {
        self.rotation_period = Some(period);
        self
    }

    /// Set the runtime flavor.
    pub fn flavor(mut self, flavor: RuntimeFlavor) -> Self {
        self.flavor = Some(flavor);
        self
    }

    /// Set the number of worker threads (multi-thread only).
    pub fn worker_threads(mut self, n: usize) -> Self {
        self.worker_threads = Some(n);
        self
    }

    /// Set to `false` to build a plain runtime with no telemetry installed.
    pub fn install(mut self, enabled: bool) -> Self {
        self.install = Some(enabled);
        self
    }

    /// Enable task spawn/terminate tracking.
    pub fn task_tracking(mut self, enabled: bool) -> Self {
        self.task_tracking = Some(enabled);
        self
    }

    /// Set a human-readable runtime name for segment metadata.
    pub fn runtime_name(mut self, name: impl Into<String>) -> Self {
        self.runtime_name = Some(name.into());
        self
    }

    /// Set how often the background worker polls for sealed segments.
    pub fn worker_poll_interval(mut self, interval: Duration) -> Self {
        self.worker_poll_interval = Some(interval);
        self
    }

    /// Finalize the builder into a [`Dial9Config`].
    ///
    /// # Panics
    ///
    /// Panics if `base_path`, `max_file_size`, or `max_total_size` have not
    /// been set.
    pub fn build(self) -> Dial9Config {
        Dial9Config {
            base_path: self.base_path.expect("base_path is required"),
            max_file_size: self.max_file_size.expect("max_file_size is required"),
            max_total_size: self.max_total_size.expect("max_total_size is required"),
            rotation_period: self.rotation_period,
            flavor: self.flavor,
            worker_threads: self.worker_threads,
            install: self.install,
            task_tracking: self.task_tracking,
            runtime_name: self.runtime_name,
            worker_poll_interval: self.worker_poll_interval,
            #[cfg(feature = "cpu-profiling")]
            cpu_profiling: self.cpu_profiling,
            #[cfg(feature = "cpu-profiling")]
            sched_events: self.sched_events,
            #[cfg(feature = "worker-s3")]
            s3_config: self.s3_config,
        }
    }
}

#[cfg(feature = "cpu-profiling")]
impl Dial9ConfigBuilder {
    /// Enable CPU profiling with the given configuration (Linux only).
    pub fn cpu_profiling(
        mut self,
        config: crate::telemetry::cpu_profile::CpuProfilingConfig,
    ) -> Self {
        self.cpu_profiling = Some(config);
        self
    }

    /// Enable per-worker scheduler event capture (Linux only).
    pub fn sched_events(mut self, config: crate::telemetry::cpu_profile::SchedEventConfig) -> Self {
        self.sched_events = Some(config);
        self
    }
}

#[cfg(feature = "worker-s3")]
impl Dial9ConfigBuilder {
    /// Configure S3 upload for sealed trace segments.
    pub fn s3_uploader(mut self, config: crate::background_task::s3::S3Config) -> Self {
        self.s3_config = Some(config);
        self
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn tmp_base_path() -> PathBuf {
        let dir = tempfile::tempdir().expect("tempdir");
        // Leak the TempDir so it isn't deleted while the test runs.
        let path = dir.path().join("trace.bin");
        std::mem::forget(dir);
        path
    }

    // --- builder validation ---

    #[test]
    fn builder_all_required_fields() {
        let config = Dial9Config::builder()
            .base_path(tmp_base_path())
            .max_file_size(1024)
            .max_total_size(4096)
            .build();
        // Should not panic — config is valid.
        let _ = config;
    }

    #[test]
    #[should_panic(expected = "base_path is required")]
    fn builder_missing_base_path_panics() {
        Dial9Config::builder()
            .max_file_size(1024)
            .max_total_size(4096)
            .build();
    }

    #[test]
    #[should_panic(expected = "max_file_size is required")]
    fn builder_missing_max_file_size_panics() {
        Dial9Config::builder()
            .base_path("/tmp/t.bin")
            .max_total_size(4096)
            .build();
    }

    #[test]
    #[should_panic(expected = "max_total_size is required")]
    fn builder_missing_max_total_size_panics() {
        Dial9Config::builder()
            .base_path("/tmp/t.bin")
            .max_file_size(1024)
            .build();
    }

    // --- build() integration ---

    #[test]
    fn build_creates_working_runtime() {
        let config = Dial9Config::builder()
            .base_path(tmp_base_path())
            .max_file_size(1024 * 1024)
            .max_total_size(4 * 1024 * 1024)
            .build();
        let (runtime, guard) = config.build().expect("build failed");
        let handle = guard.handle();
        let result = runtime.block_on(async {
            handle.spawn(async { 42 }).await.unwrap()
        });
        assert_eq!(result, 42);
    }

    #[test]
    fn build_with_install_false() {
        let config = Dial9Config::builder()
            .base_path(tmp_base_path())
            .max_file_size(1024)
            .max_total_size(4096)
            .install(false)
            .build();
        let (runtime, guard) = config.build().expect("build failed");
        let handle = guard.handle();
        let result = runtime.block_on(async {
            handle.spawn(async { 7 }).await.unwrap()
        });
        assert_eq!(result, 7);
    }

    #[test]
    fn build_current_thread_flavor() {
        let config = Dial9Config::builder()
            .base_path(tmp_base_path())
            .max_file_size(1024)
            .max_total_size(4096)
            .flavor(RuntimeFlavor::CurrentThread)
            .build();
        let (runtime, guard) = config.build().expect("build failed");
        let handle = guard.handle();
        let result = runtime.block_on(async {
            handle.spawn(async { 99 }).await.unwrap()
        });
        assert_eq!(result, 99);
    }
}
