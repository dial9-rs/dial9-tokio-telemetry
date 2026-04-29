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

use std::fmt;
use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;

use crate::telemetry::recorder::{HasTracePath, TracedRuntime, TracedRuntimeBuilder};

// ---------------------------------------------------------------------------
// Dial9ConfigBuilderError — unified error for builder validation and runtime construction
// ---------------------------------------------------------------------------

/// Errors produced while building a [`Dial9Config`].
#[derive(Debug)]
#[non_exhaustive]
pub enum Dial9ConfigBuilderError {
    /// Telemetry is enabled (the default) but one or more required writer
    /// fields were never set on the builder.
    MissingFields(MissingFields),
}

/// Opaque payload for [`Dial9ConfigBuilderError::MissingFields`].
#[derive(Debug)]
pub struct MissingFields {
    fields: Vec<&'static str>,
}

impl MissingFields {
    /// The names of the required builder setters that were not called.
    pub fn fields(&self) -> &[&'static str] {
        &self.fields
    }
}

impl std::fmt::Display for MissingFields {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(
            f,
            "missing required Dial9Config fields: {}",
            self.fields.join(", ")
        )
    }
}

impl std::fmt::Display for Dial9ConfigBuilderError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Dial9ConfigBuilderError::MissingFields(m) => write!(f, "{m}"),
        }
    }
}

impl std::error::Error for Dial9ConfigBuilderError {}

// ---------------------------------------------------------------------------
// Dial9Config — opaque value the macro consumes
// ---------------------------------------------------------------------------

/// Finalized strict configuration consumed by the `#[main]` macro.
///
/// Constructed via [`Dial9Config::builder()`] followed by
/// [`Dial9ConfigBuilder::build`]. Converting this into a
/// [`crate::TelemetryRuntime`] propagates any I/O failure as a
/// [`crate::TelemetryRuntimeError`]; nothing is hidden. For an opt-in
/// lenient variant that silently downgrades to a plain tokio runtime on
/// I/O failure, see [`Dial9ConfigFallback`].
#[derive(Debug)]
pub struct Dial9Config(pub(crate) Inner);

/// Lenient configuration variant that opts into runtime-time fallback.
///
/// Constructed via [`Dial9ConfigBuilder::build_or_disabled`]. Converting
/// this into a [`crate::TelemetryRuntime`] silently downgrades
/// `RotatingWriter` and telemetry-core I/O failures to a plain tokio
/// runtime built from the user's `with_tokio` configurators; only the
/// tokio builder's own [`std::io::Error`] can still escape.
#[derive(Debug)]
pub struct Dial9ConfigFallback(pub(crate) Inner);

/// A configurator closure that customizes a [`tokio::runtime::Builder`].
///
/// Stored as `Arc<dyn Fn ...>` so that the configurator vector is cheaply
/// cloneable (required for the fallback path's runtime cascade, which
/// re-materializes the tokio builder after `build_and_start` consumes the
/// first attempt).
pub(crate) type TokioConfigurator =
    Arc<dyn Fn(&mut tokio::runtime::Builder) + Send + Sync + 'static>;

#[allow(clippy::large_enum_variant)]
pub(crate) enum Inner {
    Enabled {
        base_path: PathBuf,
        max_file_size: u64,
        max_total_size: u64,
        rotation_period: Option<Duration>,
        tokio_configurators: Vec<TokioConfigurator>,
        runtime_builder: TracedRuntimeBuilder<HasTracePath>,
    },
    Disabled {
        tokio_configurators: Vec<TokioConfigurator>,
    },
}

impl fmt::Debug for Inner {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Inner::Enabled {
                base_path,
                max_file_size,
                max_total_size,
                rotation_period,
                tokio_configurators,
                runtime_builder,
            } => f
                .debug_struct("Enabled")
                .field("base_path", base_path)
                .field("max_file_size", max_file_size)
                .field("max_total_size", max_total_size)
                .field("rotation_period", rotation_period)
                .field("tokio_configurators", &tokio_configurators.len())
                .field("runtime_builder", runtime_builder)
                .finish(),
            Inner::Disabled {
                tokio_configurators,
            } => f
                .debug_struct("Disabled")
                .field("tokio_configurators", &tokio_configurators.len())
                .finish(),
        }
    }
}

pub(crate) fn materialize_tokio_builder(
    configurators: &[TokioConfigurator],
) -> tokio::runtime::Builder {
    let mut b = default_tokio_builder();
    for c in configurators {
        c(&mut b);
    }
    b
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
        #[builder(field)] tokio_configurators: Vec<TokioConfigurator>,

        #[builder(field)] runtime_configurators: Vec<RuntimeConfigurator>,

        /// Defaults to `true`. When `false`, required writer fields are
        /// ignored and the runtime is built without telemetry.
        #[builder(default = true)]
        enabled: bool,
        /// Trace output path.
        #[builder(into)]
        base_path: Option<PathBuf>,
        /// Per-file rotation threshold in bytes.
        max_file_size: Option<u64>,
        /// Total disk budget in bytes.
        max_total_size: Option<u64>,
        /// Wall-clock rotation period for the writer.
        rotation_period: Option<Duration>,
    ) -> Result<Dial9Config, Dial9ConfigBuilderError> {
        assemble(
            tokio_configurators,
            runtime_configurators,
            enabled,
            base_path,
            max_file_size,
            max_total_size,
            rotation_period,
        )
        .map(Dial9Config)
    }
}

/// Shared finish-fn body: builder-staged fields > [`Inner`].
///
/// Used by both `build()` (which propagates the error) and
/// `build_or_disabled()` (which substitutes [`Inner::Disabled`] on error).
#[allow(clippy::too_many_arguments)]
fn assemble(
    tokio_configurators: Vec<TokioConfigurator>,
    runtime_configurators: Vec<RuntimeConfigurator>,
    enabled: bool,
    base_path: Option<PathBuf>,
    max_file_size: Option<u64>,
    max_total_size: Option<u64>,
    rotation_period: Option<Duration>,
) -> Result<Inner, Dial9ConfigBuilderError> {
    if !enabled {
        return Ok(Inner::Disabled {
            tokio_configurators,
        });
    }

    let required_fields = (base_path, max_file_size, max_total_size);
    let (base_path, max_file_size, max_total_size) = match required_fields {
        (Some(bp), Some(mfs), Some(mts)) => (bp, mfs, mts),
        (bp, mfs, mts) => {
            let missing = [
                ("base_path", bp.is_none()),
                ("max_file_size", mfs.is_none()),
                ("max_total_size", mts.is_none()),
            ]
            .into_iter()
            .filter_map(|(name, missing)| missing.then_some(name))
            .collect();
            return Err(Dial9ConfigBuilderError::MissingFields(MissingFields {
                fields: missing,
            }));
        }
    };

    let mut runtime_builder = TracedRuntime::builder().with_trace_path(base_path.clone());
    for configure in runtime_configurators {
        runtime_builder = configure(runtime_builder);
    }

    Ok(Inner::Enabled {
        base_path,
        max_file_size,
        max_total_size,
        rotation_period,
        tokio_configurators,
        runtime_builder,
    })
}

impl<S: dial9_config_builder::State> Dial9ConfigBuilder<S> {
    /// Queue a configurator for the underlying [`tokio::runtime::Builder`].
    ///
    /// The closure receives a fresh builder by mutable reference — use any
    /// tokio knob (`worker_threads`, `thread_name`, `thread_stack_size`,
    /// `global_queue_interval`, etc.). The builder is pre-seeded with
    /// `new_multi_thread()` and `enable_all()`. To switch flavors, replace
    /// the whole builder inside the closure:
    /// `*t = tokio::runtime::Builder::new_current_thread(); t.enable_all();`.
    ///
    /// Can be called multiple times; configurators are applied in call
    /// order each time the runtime is materialized. The closure must be
    /// `Fn + Send + Sync + 'static` so that
    /// [`build_or_disabled`](Self::build_or_disabled)'s fallback path can
    /// re-materialize the builder if the primary attempt's I/O fails.
    pub fn with_tokio<F>(mut self, f: F) -> Self
    where
        F: Fn(&mut tokio::runtime::Builder) + Send + Sync + 'static,
    {
        self.tokio_configurators.push(Arc::new(f));
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

impl<S: dial9_config_builder::IsComplete> Dial9ConfigBuilder<S> {
    /// Finish into a [`Dial9ConfigFallback`] that never reports a build
    /// error.
    ///
    /// On builder validation failure (e.g. missing required writer fields),
    /// emits a [`Dial9ConfigFallback`] wrapping [`Inner::Disabled`] with
    /// the user's `with_tokio` configurators preserved. The resulting
    /// config also opts into a runtime-time cascade: any
    /// `RotatingWriter` / telemetry-core I/O failure during
    /// [`crate::TelemetryRuntime::try_from`] silently downgrades to a
    /// plain tokio runtime built from the same configurators. Only
    /// [`std::io::Error`] (from the tokio builder itself) can still
    /// escape that conversion.
    ///
    /// Counterpart to [`build`](Self::build), which preserves today's
    /// strict behavior — RotatingWriter / telemetry-core I/O failures
    /// propagate as [`crate::TelemetryRuntimeError`] from the strict path.
    pub fn build_or_disabled(self) -> Dial9ConfigFallback {
        let cfgs_for_fallback = self.tokio_configurators.clone();
        match self.build() {
            Ok(cfg) => Dial9ConfigFallback(cfg.0),
            Err(_) => Dial9ConfigFallback(Inner::Disabled {
                tokio_configurators: cfgs_for_fallback,
            }),
        }
    }
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;
    use std::sync::atomic::{AtomicUsize, Ordering};

    use crate::TelemetryRuntime;
    use crate::TelemetryRuntimeError;

    use super::*;

    fn tmp_base_path() -> PathBuf {
        let dir = tempfile::tempdir().expect("tempdir");
        // Leak the TempDir so it isn't deleted while the test runs.
        let path = dir.path().join("trace.bin");
        std::mem::forget(dir);
        path
    }

    /// A path under a directory that does not exist; RotatingWriter::build()
    /// will fail to create the trace file there.
    fn unwritable_base_path() -> PathBuf {
        PathBuf::from("/this/dir/does/not/exist/dial9_test_trace.bin")
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
    fn missing_required_fields_errors_with_all_missing_names() {
        match Dial9Config::builder().build() {
            Err(Dial9ConfigBuilderError::MissingFields(m)) => {
                assert_eq!(m.fields(), ["base_path", "max_file_size", "max_total_size"]);
            }
            Ok(_) => panic!("expected MissingFields error, got Ok"),
        }
    }

    #[test]
    fn missing_some_required_fields_lists_only_missing() {
        match Dial9Config::builder().max_file_size(1024).build() {
            Err(Dial9ConfigBuilderError::MissingFields(m)) => {
                assert_eq!(m.fields(), ["base_path", "max_total_size"]);
            }
            Ok(_) => panic!("expected MissingFields error, got Ok"),
        }
    }

    #[test]
    fn explicitly_enabled_still_requires_fields() {
        match Dial9Config::builder().enabled(true).build() {
            Err(Dial9ConfigBuilderError::MissingFields(_)) => {}
            Ok(_) => panic!("expected MissingFields error, got Ok"),
        }
    }

    // ---------------------------------------------------------------
    // build_or_disabled
    // ---------------------------------------------------------------

    #[test]
    fn build_or_disabled_from_incomplete_builder_yields_disabled_runtime() {
        let cfg = Dial9Config::builder().build_or_disabled();
        let rt = TelemetryRuntime::try_from(cfg).expect("fallback runtime should build");
        assert!(
            rt.guard().is_none(),
            "fallback path must not install a guard"
        );
        let v = rt.block_on(async { 7u32 });
        assert_eq!(v, 7);
    }

    #[test]
    fn build_or_disabled_from_complete_builder_yields_enabled_runtime() {
        let cfg = Dial9Config::builder()
            .base_path(tmp_base_path())
            .max_file_size(1024)
            .max_total_size(4096)
            .build_or_disabled();
        let rt = TelemetryRuntime::try_from(cfg).expect("enabled runtime should build");
        assert!(
            rt.guard().is_some(),
            "valid config must keep telemetry enabled"
        );
    }

    #[test]
    fn build_or_disabled_cascades_runtime_io_failure_to_disabled() {
        let cfg = Dial9Config::builder()
            .base_path(unwritable_base_path())
            .max_file_size(1024)
            .max_total_size(4096)
            .build_or_disabled();
        let rt = TelemetryRuntime::try_from(cfg)
            .expect("RotatingWriter failure should cascade to disabled");
        assert!(
            rt.guard().is_none(),
            "cascade path must not install a guard"
        );
        let v = rt.block_on(async { 42u32 });
        assert_eq!(v, 42);
    }

    #[test]
    fn build_or_disabled_replays_with_tokio_configurators_on_cascade() {
        let counter = Arc::new(AtomicUsize::new(0));
        let counter_for_closure = Arc::clone(&counter);
        let cfg = Dial9Config::builder()
            .base_path(unwritable_base_path())
            .max_file_size(1024)
            .max_total_size(4096)
            .with_tokio(move |b| {
                counter_for_closure.fetch_add(1, Ordering::SeqCst);
                b.worker_threads(1);
            })
            .build_or_disabled();
        let rt = TelemetryRuntime::try_from(cfg).expect("cascade should produce a runtime");
        assert!(rt.guard().is_none());
        let calls = counter.load(Ordering::SeqCst);
        assert!(
            calls >= 1,
            "with_tokio configurator must run on the fallback runtime build (was {calls})"
        );
    }

    #[test]
    fn strict_build_still_bubbles_rotating_writer_error_on_unwritable_path() {
        let cfg = Dial9Config::builder()
            .base_path(unwritable_base_path())
            .max_file_size(1024)
            .max_total_size(4096)
            .build()
            .expect("validation passes; only runtime I/O should fail");
        let result = TelemetryRuntime::try_from(cfg);
        assert!(
            matches!(result, Err(TelemetryRuntimeError::RotatingWriter(_))),
            "strict path must propagate RotatingWriter error, got {result:?}"
        );
    }
}
