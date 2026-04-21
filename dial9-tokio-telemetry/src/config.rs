//! Unified configuration for the `#[dial9_tokio_telemetry::main]` macro.
//!
//! Start with [`Dial9Config::builder()`] and chain setters to produce a
//! [`Dial9Config`] that the macro consumes. The builder stages a
//! [`tokio::runtime::Builder`] and accumulates [`TracedRuntimeBuilder`]
//! configurators eagerly; use [`Dial9ConfigBuilder::with_tokio`] and
//! [`Dial9ConfigBuilder::with_runtime`] to reach any knob those builders
//! expose.
//!
//! To run without telemetry while preserving tokio knobs, call
//! `.enabled(false)` — the builder then skips required-field validation
//! and any queued runtime configurators are ignored.

use std::path::PathBuf;
use std::time::Duration;

use crate::telemetry::recorder::{
    HasTracePath, TelemetryGuard, TracedRuntime, TracedRuntimeBuilder,
};
use crate::telemetry::writer::RotatingWriter;

// ---------------------------------------------------------------------------
// Dial9Error — unified error for builder validation and runtime construction
// ---------------------------------------------------------------------------

/// Errors produced while building a [`Dial9Config`] or its tokio runtime.
#[derive(Debug)]
pub enum Dial9Error {
    /// Telemetry is enabled (the default) but one or more required writer
    /// fields were never set on the builder.
    MissingFields(Vec<&'static str>),
    /// Failure from the tokio runtime builder, the rotating writer, or the
    /// telemetry core.
    Io(std::io::Error),
}

impl std::fmt::Display for Dial9Error {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Dial9Error::MissingFields(fields) => write!(
                f,
                "missing required Dial9Config fields: {}",
                fields.join(", ")
            ),
            Dial9Error::Io(e) => write!(f, "{e}"),
        }
    }
}

impl std::error::Error for Dial9Error {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            Dial9Error::Io(e) => Some(e),
            Dial9Error::MissingFields(_) => None,
        }
    }
}

impl From<std::io::Error> for Dial9Error {
    fn from(e: std::io::Error) -> Self {
        Self::Io(e)
    }
}

// ---------------------------------------------------------------------------
// Dial9Config — opaque value the macro consumes
// ---------------------------------------------------------------------------

/// Finalized configuration consumed by the `#[main]` macro.
///
/// Constructed via [`Dial9Config::builder()`].
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
    /// Returns `Some(guard)` when telemetry is enabled, `None` when the
    /// config was built with `.enabled(false)`.
    pub fn build(self) -> Result<(tokio::runtime::Runtime, Option<TelemetryGuard>), Dial9Error> {
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
// Dial9ConfigBuilder — single bon-generated fluent entry point
// ---------------------------------------------------------------------------

type RuntimeConfigurator =
    Box<dyn FnOnce(TracedRuntimeBuilder<HasTracePath>) -> TracedRuntimeBuilder<HasTracePath>>;

fn default_tokio_builder() -> tokio::runtime::Builder {
    let mut b = tokio::runtime::Builder::new_multi_thread();
    b.enable_all();
    b
}

#[bon::bon]
impl Dial9Config {
    /// Start a fluent configuration chain.
    ///
    /// When telemetry is enabled (the default), the following setters are
    /// required: [`base_path`](Dial9ConfigBuilder::base_path),
    /// [`max_file_size`](Dial9ConfigBuilder::max_file_size),
    /// [`max_total_size`](Dial9ConfigBuilder::max_total_size). They may be
    /// omitted if `.enabled(false)` is set, in which case a plain tokio
    /// runtime is built without telemetry.
    #[builder(
        builder_type = Dial9ConfigBuilder,
        finish_fn = build,
        state_mod = dial9_config_builder,
    )]
    pub fn builder(
        #[builder(field = default_tokio_builder())] tokio_builder: tokio::runtime::Builder,

        #[builder(field)] runtime_configurators: Vec<RuntimeConfigurator>,

        /// Defaults to `true`. When `false`, required writer fields are
        /// ignored and the runtime is built without telemetry.
        enabled: Option<bool>,
        /// Trace output path.
        #[builder(into)]
        base_path: Option<PathBuf>,
        /// Per-file rotation threshold in bytes.
        max_file_size: Option<u64>,
        /// Total disk budget in bytes.
        max_total_size: Option<u64>,
        /// Wall-clock rotation period for the writer.
        rotation_period: Option<Duration>,
    ) -> Result<Dial9Config, Dial9Error> {
        let enabled = enabled.unwrap_or(true);
        if !enabled {
            return Ok(Dial9Config(Inner::Disabled { tokio_builder }));
        }

        let (base_path, max_file_size, max_total_size) =
            match (base_path, max_file_size, max_total_size) {
                (Some(bp), Some(mfs), Some(mts)) => (bp, mfs, mts),
                (bp, mfs, mts) => {
                    let mut missing: Vec<&'static str> = Vec::new();
                    if bp.is_none() {
                        missing.push("base_path");
                    }
                    if mfs.is_none() {
                        missing.push("max_file_size");
                    }
                    if mts.is_none() {
                        missing.push("max_total_size");
                    }
                    return Err(Dial9Error::MissingFields(missing));
                }
            };

        let mut runtime_builder = TracedRuntime::builder().with_trace_path(base_path.clone());
        for configure in runtime_configurators {
            runtime_builder = configure(runtime_builder);
        }

        Ok(Dial9Config(Inner::Enabled {
            base_path,
            max_file_size,
            max_total_size,
            rotation_period,
            tokio_builder,
            runtime_builder,
        }))
    }
}

impl<S: dial9_config_builder::State> Dial9ConfigBuilder<S> {
    /// Customize the underlying [`tokio::runtime::Builder`].
    ///
    /// The closure receives the staged builder by mutable reference — use
    /// any tokio knob (`worker_threads`, `thread_name`, `thread_stack_size`,
    /// `global_queue_interval`, etc.). The builder is pre-seeded with
    /// `new_multi_thread()` and `enable_all()`. To switch flavors, replace
    /// the whole builder inside the closure:
    /// `*t = tokio::runtime::Builder::new_current_thread(); t.enable_all();`.
    ///
    /// Can be called multiple times; mutations compose in call order.
    pub fn with_tokio<F>(mut self, f: F) -> Self
    where
        F: FnOnce(&mut tokio::runtime::Builder),
    {
        f(&mut self.tokio_builder);
        self
    }

    /// Queue a configurator for the dial9 [`TracedRuntimeBuilder`].
    ///
    /// The closure receives the staged builder by value and must return it.
    /// Use this to access runtime configuration methods like
    /// `with_runtime_name` and `with_task_tracking`; see
    /// [`TracedRuntimeBuilder`] for the full list.
    ///
    /// Queued configurators are applied in call order during `build()`
    /// once `base_path` is known. When `.enabled(false)` is set, queued
    /// configurators are ignored.
    pub fn with_runtime<F>(mut self, f: F) -> Self
    where
        F: FnOnce(TracedRuntimeBuilder<HasTracePath>) -> TracedRuntimeBuilder<HasTracePath>
            + 'static,
    {
        self.runtime_configurators.push(Box::new(f));
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

    #[test]
    fn builder_accepts_required_fields() {
        let _ = Dial9Config::builder()
            .base_path(tmp_base_path())
            .max_file_size(1024)
            .max_total_size(4096)
            .build()
            .expect("build should succeed");
    }

    #[test]
    fn build_creates_working_runtime() {
        let config = Dial9Config::builder()
            .base_path(tmp_base_path())
            .max_file_size(1024 * 1024)
            .max_total_size(4 * 1024 * 1024)
            .build()
            .expect("config build failed");
        let (runtime, guard) = config.build().expect("runtime build failed");
        let guard = guard.expect("guard should be Some for enabled config");
        let handle = guard.handle();
        let result = runtime.block_on(async { handle.spawn(async { 42 }).await.unwrap() });
        assert_eq!(result, 42);
    }

    #[test]
    fn with_runtime_install_false() {
        let config = Dial9Config::builder()
            .base_path(tmp_base_path())
            .max_file_size(1024)
            .max_total_size(4096)
            .with_runtime(|r| r.install(false))
            .build()
            .expect("config build failed");
        let (runtime, guard) = config.build().expect("runtime build failed");
        let guard = guard.expect("guard should be Some");
        let handle = guard.handle();
        let result = runtime.block_on(async { handle.spawn(async { 7 }).await.unwrap() });
        assert_eq!(result, 7);
    }

    #[test]
    fn with_tokio_current_thread() {
        let config = Dial9Config::builder()
            .base_path(tmp_base_path())
            .max_file_size(1024)
            .max_total_size(4096)
            .with_tokio(|t| {
                *t = tokio::runtime::Builder::new_current_thread();
                t.enable_all();
            })
            .build()
            .expect("config build failed");
        let (runtime, guard) = config.build().expect("runtime build failed");
        let guard = guard.expect("guard should be Some");
        let handle = guard.handle();
        let result = runtime.block_on(async { handle.spawn(async { 99 }).await.unwrap() });
        assert_eq!(result, 99);
    }

    #[test]
    fn with_tokio_worker_threads() {
        let config = Dial9Config::builder()
            .base_path(tmp_base_path())
            .max_file_size(1024)
            .max_total_size(4096)
            .with_tokio(|t| {
                t.worker_threads(2);
            })
            .build()
            .expect("config build failed");
        let (runtime, guard) = config.build().expect("runtime build failed");
        let guard = guard.expect("guard should be Some");
        let handle = guard.handle();
        let result = runtime.block_on(async { handle.spawn(async { 3 }).await.unwrap() });
        assert_eq!(result, 3);
    }

    #[test]
    fn with_runtime_chained_knobs() {
        let config = Dial9Config::builder()
            .base_path(tmp_base_path())
            .max_file_size(1024)
            .max_total_size(4096)
            .with_runtime(|r| r.with_runtime_name("test-rt").with_task_tracking(true))
            .build()
            .expect("config build failed");
        let (runtime, guard) = config.build().expect("runtime build failed");
        let guard = guard.expect("guard should be Some");
        let handle = guard.handle();
        let result = runtime.block_on(async { handle.spawn(async { 1 }).await.unwrap() });
        assert_eq!(result, 1);
    }

    #[test]
    fn multiple_with_runtime_calls_all_apply() {
        let config = Dial9Config::builder()
            .base_path(tmp_base_path())
            .max_file_size(1024)
            .max_total_size(4096)
            .with_runtime(|r| r.with_runtime_name("first"))
            .with_runtime(|r| r.with_task_tracking(true))
            .build()
            .expect("config build failed");
        let (runtime, guard) = config.build().expect("runtime build failed");
        let guard = guard.expect("guard should be Some");
        let handle = guard.handle();
        let result = runtime.block_on(async { handle.spawn(async { 11 }).await.unwrap() });
        assert_eq!(result, 11);
    }

    #[test]
    fn disabled_builds_plain_runtime() {
        let config = Dial9Config::builder()
            .enabled(false)
            .with_tokio(|t| {
                t.worker_threads(2);
            })
            .build()
            .expect("config build failed");
        let (runtime, guard) = config.build().expect("runtime build failed");
        assert!(guard.is_none(), "guard should be None for disabled config");
        let result = runtime.block_on(async { tokio::spawn(async { 55 }).await.unwrap() });
        assert_eq!(result, 55);
    }

    #[test]
    fn disabled_needs_no_required_fields() {
        let config = Dial9Config::builder()
            .enabled(false)
            .build()
            .expect("config build failed");
        let (runtime, guard) = config.build().expect("runtime build failed");
        assert!(guard.is_none());
        let result = runtime.block_on(async { tokio::spawn(async { 77 }).await.unwrap() });
        assert_eq!(result, 77);
    }

    #[test]
    fn missing_required_fields_errors_with_all_missing_names() {
        match Dial9Config::builder().build() {
            Err(Dial9Error::MissingFields(fields)) => {
                assert_eq!(fields, vec!["base_path", "max_file_size", "max_total_size"]);
            }
            Err(other) => panic!("expected MissingFields, got {other:?}"),
            Ok(_) => panic!("expected MissingFields error, got Ok"),
        }
    }

    #[test]
    fn missing_some_required_fields_lists_only_missing() {
        match Dial9Config::builder().max_file_size(1024).build() {
            Err(Dial9Error::MissingFields(fields)) => {
                assert_eq!(fields, vec!["base_path", "max_total_size"]);
            }
            Err(other) => panic!("expected MissingFields, got {other:?}"),
            Ok(_) => panic!("expected MissingFields error, got Ok"),
        }
    }

    #[test]
    fn explicitly_enabled_still_requires_fields() {
        match Dial9Config::builder().enabled(true).build() {
            Err(Dial9Error::MissingFields(_)) => {}
            Err(other) => panic!("expected MissingFields, got {other:?}"),
            Ok(_) => panic!("expected MissingFields error, got Ok"),
        }
    }

    #[test]
    fn with_runtime_before_base_path_still_builds() {
        let config = Dial9Config::builder()
            .with_runtime(|r| r.with_runtime_name("early"))
            .base_path(tmp_base_path())
            .max_file_size(1024)
            .max_total_size(4096)
            .build()
            .expect("config build failed");
        let (runtime, guard) = config.build().expect("runtime build failed");
        let guard = guard.expect("guard should be Some");
        let handle = guard.handle();
        let result = runtime.block_on(async { handle.spawn(async { 5 }).await.unwrap() });
        assert_eq!(result, 5);
    }
}
