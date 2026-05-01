//! Positional-argument config API for the
//! `#[dial9_tokio_telemetry::main]` macro.
//!
//! The fluent builder re-exported at the crate root
//! (see [`crate::Dial9Config::builder`]) is a more ergonomic alternative
//! and tends to read more clearly when there are several knobs to set.
//!
//! Equivalent calls on the fluent builder, we encourage you to migrate to:
//! - `Dial9ConfigBuilder::new(..)` →
//!   `Dial9Config::builder().base_path(..).max_file_size(..).max_total_size(..)`.
//! - `Dial9ConfigBuilder::disabled()` →
//!   `Dial9Config::builder().enabled(false)`.
//! - `.with_tokio()` / `.with_runtime()` are unchanged.

use std::path::PathBuf;
use std::time::Duration;

use crate::telemetry::recorder::{
    HasTracePath, TelemetryGuard, TracedRuntime, TracedRuntimeBuilder,
};
use crate::telemetry::writer::RotatingWriter;

// ---------------------------------------------------------------------------
// Dial9Config — opaque value the macro consumes
// ---------------------------------------------------------------------------

/// Finalized configuration consumed by the `#[main]` macro.
///
/// Constructed via [`Dial9ConfigBuilder::build`] or
/// [`DisabledDial9ConfigBuilder::build`].
#[allow(missing_debug_implementations)]
pub struct Dial9Config(Inner);

#[allow(clippy::large_enum_variant)]
enum Inner {
    Enabled {
        base_path: PathBuf,
        max_file_size: u64,
        max_total_size: u64,
        rotation_period: Option<Duration>,
        tokio_builder: tokio::runtime::Builder,
        runtime_builder: TracedRuntimeBuilder<HasTracePath>,
    },
    Disabled {
        tokio_builder: tokio::runtime::Builder,
    },
}

impl Dial9Config {
    /// Build the tokio runtime, optionally with dial9 telemetry installed.
    ///
    /// Returns `Some(guard)` when telemetry is enabled, `None` when built
    /// from [`DisabledDial9ConfigBuilder`].
    pub fn build(self) -> std::io::Result<(tokio::runtime::Runtime, Option<TelemetryGuard>)> {
        match self.0 {
            Inner::Enabled {
                base_path,
                max_file_size,
                max_total_size,
                rotation_period,
                tokio_builder,
                runtime_builder,
            } => {
                let writer = RotatingWriter::builder()
                    .base_path(base_path)
                    .max_file_size(max_file_size)
                    .max_total_size(max_total_size)
                    .maybe_rotation_period(rotation_period)
                    .build()?;
                let (runtime, guard) = runtime_builder.build_and_start(tokio_builder, writer)?;
                Ok((runtime, Some(guard)))
            }
            Inner::Disabled { mut tokio_builder } => {
                let runtime = tokio_builder.build()?;
                Ok((runtime, None))
            }
        }
    }
}

// ---------------------------------------------------------------------------
// Dial9ConfigBuilder — enabled path
// ---------------------------------------------------------------------------

/// Builder for a [`Dial9Config`] with telemetry enabled.
///
/// Created via [`Dial9ConfigBuilder::new`]. Exposes both tokio and dial9
/// runtime knobs. Call [`.build()`](Self::build) to produce a [`Dial9Config`].
#[derive(Debug)]
pub struct Dial9ConfigBuilder {
    base_path: PathBuf,
    max_file_size: u64,
    max_total_size: u64,
    rotation_period: Option<Duration>,
    tokio_builder: tokio::runtime::Builder,
    runtime_builder: TracedRuntimeBuilder<HasTracePath>,
}

impl Dial9ConfigBuilder {
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

    /// Create a [`DisabledDial9ConfigBuilder`] that builds a plain tokio
    /// runtime with no telemetry. Only `.with_tokio()` is available.
    pub fn disabled() -> DisabledDial9ConfigBuilder {
        DisabledDial9ConfigBuilder::new()
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

    /// Finalize into a [`Dial9Config`] ready for the macro.
    pub fn build(self) -> Dial9Config {
        Dial9Config(Inner::Enabled {
            base_path: self.base_path,
            max_file_size: self.max_file_size,
            max_total_size: self.max_total_size,
            rotation_period: self.rotation_period,
            tokio_builder: self.tokio_builder,
            runtime_builder: self.runtime_builder,
        })
    }
}

// ---------------------------------------------------------------------------
// DisabledDial9ConfigBuilder — disabled path (tokio-only)
// ---------------------------------------------------------------------------

/// Builder for a [`Dial9Config`] with telemetry disabled.
///
/// Created via [`Dial9ConfigBuilder::disabled()`]. Only exposes tokio
/// runtime knobs — telemetry methods like `with_runtime` are not available.
#[derive(Debug)]
pub struct DisabledDial9ConfigBuilder {
    tokio_builder: tokio::runtime::Builder,
}

impl DisabledDial9ConfigBuilder {
    fn new() -> Self {
        let mut tokio_builder = tokio::runtime::Builder::new_multi_thread();
        tokio_builder.enable_all();
        Self { tokio_builder }
    }

    /// Customize the underlying [`tokio::runtime::Builder`].
    ///
    /// See [`Dial9ConfigBuilder::with_tokio`] for details.
    pub fn with_tokio<F>(mut self, f: F) -> Self
    where
        F: FnOnce(&mut tokio::runtime::Builder),
    {
        f(&mut self.tokio_builder);
        self
    }

    /// Finalize into a [`Dial9Config`] ready for the macro.
    pub fn build(self) -> Dial9Config {
        Dial9Config(Inner::Disabled {
            tokio_builder: self.tokio_builder,
        })
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
        let _ = Dial9ConfigBuilder::new(tmp_base_path(), 1024, 4096);
    }

    #[test]
    fn build_creates_working_runtime() {
        let config = Dial9ConfigBuilder::new(tmp_base_path(), 1024 * 1024, 4 * 1024 * 1024).build();
        let (runtime, guard) = config.build().expect("build failed");
        let guard = guard.expect("guard should be Some for enabled config");
        let handle = guard.handle();
        let result = runtime.block_on(async { handle.spawn(async { 42 }).await.unwrap() });
        assert_eq!(result, 42);
    }

    #[test]
    fn with_runtime_install_false() {
        let config = Dial9ConfigBuilder::new(tmp_base_path(), 1024, 4096)
            .with_runtime(|r| r.install(false))
            .build();
        let (runtime, guard) = config.build().expect("build failed");
        let guard = guard.expect("guard should be Some");
        let handle = guard.handle();
        let result = runtime.block_on(async { handle.spawn(async { 7 }).await.unwrap() });
        assert_eq!(result, 7);
    }

    #[test]
    fn with_tokio_current_thread() {
        let config = Dial9ConfigBuilder::new(tmp_base_path(), 1024, 4096)
            .with_tokio(|t| {
                *t = tokio::runtime::Builder::new_current_thread();
                t.enable_all();
            })
            .build();
        let (runtime, guard) = config.build().expect("build failed");
        let guard = guard.expect("guard should be Some");
        let handle = guard.handle();
        let result = runtime.block_on(async { handle.spawn(async { 99 }).await.unwrap() });
        assert_eq!(result, 99);
    }

    #[test]
    fn with_tokio_worker_threads() {
        let config = Dial9ConfigBuilder::new(tmp_base_path(), 1024, 4096)
            .with_tokio(|t| {
                t.worker_threads(2);
            })
            .build();
        let (runtime, guard) = config.build().expect("build failed");
        let guard = guard.expect("guard should be Some");
        let handle = guard.handle();
        let result = runtime.block_on(async { handle.spawn(async { 3 }).await.unwrap() });
        assert_eq!(result, 3);
    }

    #[test]
    fn with_runtime_chained_knobs() {
        let config = Dial9ConfigBuilder::new(tmp_base_path(), 1024, 4096)
            .with_runtime(|r| r.with_runtime_name("test-rt").with_task_tracking(true))
            .build();
        let (runtime, guard) = config.build().expect("build failed");
        let guard = guard.expect("guard should be Some");
        let handle = guard.handle();
        let result = runtime.block_on(async { handle.spawn(async { 1 }).await.unwrap() });
        assert_eq!(result, 1);
    }

    #[test]
    fn disabled_builds_plain_runtime() {
        let config = Dial9ConfigBuilder::disabled()
            .with_tokio(|t| {
                t.worker_threads(2);
            })
            .build();
        let (runtime, guard) = config.build().expect("build failed");
        assert!(guard.is_none(), "guard should be None for disabled config");
        let result = runtime.block_on(async { tokio::spawn(async { 55 }).await.unwrap() });
        assert_eq!(result, 55);
    }

    #[test]
    fn disabled_default() {
        let config = Dial9ConfigBuilder::disabled().build();
        let (runtime, guard) = config.build().expect("build failed");
        assert!(guard.is_none());
        let result = runtime.block_on(async { tokio::spawn(async { 77 }).await.unwrap() });
        assert_eq!(result, 77);
    }
}
