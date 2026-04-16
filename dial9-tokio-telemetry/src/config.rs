//! Unified configuration for the `#[dial9_tokio_telemetry::main]` macro.
//!
//! [`Dial9Config`] bundles writer, tokio, and telemetry settings so that the
//! macro can construct everything from one configuration function.
//!
//! The config stages a `tokio::runtime::Builder` and a `TracedRuntimeBuilder`
//! eagerly; use [`Dial9Config::with_tokio`] and [`Dial9Config::with_runtime`]
//! to reach any knob those builders expose — including knobs that are not
//! mirrored directly on this type.

use std::path::PathBuf;
use std::time::Duration;

use crate::telemetry::recorder::{
    HasTracePath, TelemetryGuard, TracedRuntime, TracedRuntimeBuilder,
};
use crate::telemetry::writer::RotatingWriter;

/// Unified configuration produced by a user-supplied config function and
/// consumed by the `#[main]` macro to build the traced runtime.
///
/// Stages a `tokio::runtime::Builder` and a [`TracedRuntimeBuilder`]
/// internally. Reach any of their knobs via [`Self::with_tokio`] and
/// [`Self::with_runtime`] — nothing is lost relative to constructing the
/// sub-builders by hand.
#[derive(Debug)]
pub struct Dial9Config {
    base_path: PathBuf,
    max_file_size: u64,
    max_total_size: u64,
    rotation_period: Option<Duration>,
    tokio_builder: tokio::runtime::Builder,
    runtime_builder: TracedRuntimeBuilder<HasTracePath>,
}

impl Dial9Config {
    /// Start a new configuration with the three required writer fields.
    ///
    /// * `base_path` — trace file path
    /// * `max_file_size` — per-file rotation threshold in bytes
    /// * `max_total_size` — total disk budget in bytes
    pub fn new(base_path: impl Into<PathBuf>, max_file_size: u64, max_total_size: u64) -> Self {
        let base_path = base_path.into();
        let mut tokio_builder = tokio::runtime::Builder::new_multi_thread();
        tokio_builder.enable_all();
        let runtime_builder = TracedRuntime::builder().with_trace_path(base_path.clone());
        Self {
            base_path,
            max_file_size,
            max_total_size,
            rotation_period: None,
            tokio_builder,
            runtime_builder,
        }
    }

    /// Set the time-based rotation period for the writer.
    pub fn rotation_period(mut self, period: Duration) -> Self {
        self.rotation_period = Some(period);
        self
    }

    /// Customize the dial9 [`TracedRuntimeBuilder`].
    ///
    /// The closure receives the staged builder by value and must return it.
    /// Use this to access runtime configuration methods like
    /// `with_runtime_name` and `with_task_tracking`; see
    /// [`TracedRuntimeBuilder`] for the full list.
    ///
    /// Can be called multiple times; each call composes onto the prior state.
    pub fn with_runtime<F>(mut self, f: F) -> Self
    where
        F: FnOnce(TracedRuntimeBuilder<HasTracePath>) -> TracedRuntimeBuilder<HasTracePath>,
    {
        self.runtime_builder = f(self.runtime_builder);
        self
    }

    /// Customize the underlying [`tokio::runtime::Builder`].
    ///
    /// The closure receives the staged builder by mutable reference — use
    /// any tokio knob (`worker_threads`, `thread_name`, `thread_stack_size`,
    /// `global_queue_interval`, etc.). The builder is pre-seeded with
    /// `enable_all()` and `new_multi_thread()`. To switch flavors, replace
    /// the whole builder inside the closure:
    /// `*t = tokio::runtime::Builder::new_current_thread(); t.enable_all();`.
    ///
    /// Can be called multiple times; each call composes onto the prior state.
    pub fn with_tokio<F>(mut self, f: F) -> Self
    where
        F: FnOnce(&mut tokio::runtime::Builder),
    {
        f(&mut self.tokio_builder);
        self
    }

    /// Build the tokio runtime with dial9 telemetry installed and recording
    /// enabled.
    pub fn build(self) -> std::io::Result<(tokio::runtime::Runtime, TelemetryGuard)> {
        let writer = RotatingWriter::builder()
            .base_path(self.base_path)
            .max_file_size(self.max_file_size)
            .max_total_size(self.max_total_size)
            .maybe_rotation_period(self.rotation_period)
            .build()?;
        self.runtime_builder
            .build_and_start(self.tokio_builder, writer)
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

    #[test]
    fn new_all_required_fields() {
        // Should not panic — required args are positional.
        let _ = Dial9Config::new(tmp_base_path(), 1024, 4096);
    }

    #[test]
    fn build_creates_working_runtime() {
        let config = Dial9Config::new(tmp_base_path(), 1024 * 1024, 4 * 1024 * 1024);
        let (runtime, guard) = config.build().expect("build failed");
        let handle = guard.handle();
        let result = runtime.block_on(async { handle.spawn(async { 42 }).await.unwrap() });
        assert_eq!(result, 42);
    }

    #[test]
    fn with_runtime_install_false() {
        let config =
            Dial9Config::new(tmp_base_path(), 1024, 4096).with_runtime(|r| r.install(false));
        let (runtime, guard) = config.build().expect("build failed");
        let handle = guard.handle();
        let result = runtime.block_on(async { handle.spawn(async { 7 }).await.unwrap() });
        assert_eq!(result, 7);
    }

    #[test]
    fn with_tokio_current_thread() {
        let config = Dial9Config::new(tmp_base_path(), 1024, 4096).with_tokio(|t| {
            *t = tokio::runtime::Builder::new_current_thread();
            t.enable_all();
        });
        let (runtime, guard) = config.build().expect("build failed");
        let handle = guard.handle();
        let result = runtime.block_on(async { handle.spawn(async { 99 }).await.unwrap() });
        assert_eq!(result, 99);
    }

    #[test]
    fn with_tokio_worker_threads() {
        let config = Dial9Config::new(tmp_base_path(), 1024, 4096).with_tokio(|t| {
            t.worker_threads(2);
        });
        let (runtime, guard) = config.build().expect("build failed");
        let handle = guard.handle();
        let result = runtime.block_on(async { handle.spawn(async { 3 }).await.unwrap() });
        assert_eq!(result, 3);
    }

    #[test]
    fn with_runtime_chained_knobs() {
        let config = Dial9Config::new(tmp_base_path(), 1024, 4096)
            .with_runtime(|r| r.with_runtime_name("test-rt").with_task_tracking(true));
        let (runtime, guard) = config.build().expect("build failed");
        let handle = guard.handle();
        let result = runtime.block_on(async { handle.spawn(async { 1 }).await.unwrap() });
        assert_eq!(result, 1);
    }
}
