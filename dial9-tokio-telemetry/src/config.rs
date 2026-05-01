//! Unified configuration for the `#[dial9_tokio_telemetry::main]` macro.
//!
//! Start with [`Dial9Config::builder()`] and chain setters to produce a
//! config that the macro (or [`crate::TracedRuntime`]) consumes. The
//! builder stages a [`tokio::runtime::Builder`] and accumulates
//! [`TracedRuntimeBuilder`] configurators eagerly; use
//! [`Dial9ConfigBuilder::with_tokio`] and
//! [`Dial9ConfigBuilder::with_runtime`] to reach any knob those builders
//! expose.
//!
//! Two finish functions cover the strict / lenient axis:
//!
//! - [`Dial9ConfigBuilder::build`] — strict. Returns a
//!   `Result<Dial9Config, Dial9ConfigBuilderError>`. Both required-field
//!   validation and the writer's I/O probing happen here, so any error
//!   surfaces at config-build time before the runtime is touched.
//! - [`Dial9ConfigBuilder::build_or_disabled`] — lenient. Returns a
//!   [`Dial9Config`] that is *infallible at build time*: validation or
//!   I/O failures are logged at `error!` level and downgraded to a
//!   disabled config that still carries the user's `with_tokio`
//!   configurators.
//!
//! To run without telemetry while preserving tokio knobs, call
//! `.enabled(false)` — the builder then skips required-field validation
//! and any queued runtime configurators are ignored.

use std::fmt;
use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;

use crate::telemetry::recorder::{HasTracePath, TracedRuntime, TracedRuntimeBuilder};
use crate::telemetry::writer::RotatingWriter;

// ---------------------------------------------------------------------------
// Dial9ConfigBuilderError — unified error for builder validation and writer I/O
// ---------------------------------------------------------------------------

/// Errors produced while building a [`Dial9Config`].
#[derive(Debug)]
#[non_exhaustive]
pub enum Dial9ConfigBuilderError {
    /// Telemetry is enabled (the default) but one or more required writer
    /// fields were never set on the builder.
    Validation(ValidationError),
    /// Failure constructing the [`RotatingWriter`] backing telemetry — for
    /// example, an unwritable `base_path`.
    Io(std::io::Error),
}

/// Opaque payload for [`Dial9ConfigBuilderError::Validation`].
#[derive(Debug)]
pub struct ValidationError {
    fields: Vec<&'static str>,
}

impl ValidationError {
    /// The names of the required builder setters that were not called.
    pub fn fields(&self) -> &[&'static str] {
        &self.fields
    }
}

impl std::fmt::Display for ValidationError {
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
            Dial9ConfigBuilderError::Validation(v) => write!(f, "{v}"),
            Dial9ConfigBuilderError::Io(e) => write!(f, "rotating writer: {e}"),
        }
    }
}

impl std::error::Error for Dial9ConfigBuilderError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            Dial9ConfigBuilderError::Validation(_) => None,
            Dial9ConfigBuilderError::Io(e) => Some(e),
        }
    }
}

// ---------------------------------------------------------------------------
// Dial9Config — opaque value the macro consumes
// ---------------------------------------------------------------------------

/// Finalized configuration consumed by the `#[main]` macro.
///
/// Constructed via [`Dial9Config::builder()`] followed by either:
///
/// - [`Dial9ConfigBuilder::build`] — strict; returns
///   `Result<Dial9Config, Dial9ConfigBuilderError>`. The
///   [`RotatingWriter`] is probed eagerly inside `build`, so any I/O
///   failure surfaces here rather than later when the runtime is built.
/// - [`Dial9ConfigBuilder::build_or_disabled`] — lenient; never reports
///   a build error, downgrades to a disabled config that preserves the
///   user's `with_tokio` configurators on validation or I/O failure.
#[derive(Debug)]
pub struct Dial9Config(pub(crate) Inner);

/// A configurator closure that customizes a [`tokio::runtime::Builder`].
///
/// Stored as `Arc<dyn Fn ...>` so that the configurator vector is cheaply
/// cloneable — the `build_or_disabled` path needs to preserve the
/// configurators on the disabled-fallback variant when validation or
/// writer-I/O setup fails.
pub(crate) type TokioConfigurator =
    Arc<dyn Fn(&mut tokio::runtime::Builder) + Send + Sync + 'static>;

#[allow(clippy::large_enum_variant)]
pub(crate) enum Inner {
    Enabled {
        writer: RotatingWriter,
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
                writer,
                tokio_configurators,
                runtime_builder,
            } => f
                .debug_struct("Enabled")
                .field("writer", writer)
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
        assemble(AssembleArgs {
            tokio_configurators,
            runtime_configurators,
            enabled,
            base_path,
            max_file_size,
            max_total_size,
            rotation_period,
        })
        .map(Dial9Config)
    }
}

struct AssembleArgs {
    tokio_configurators: Vec<TokioConfigurator>,
    runtime_configurators: Vec<RuntimeConfigurator>,
    enabled: bool,
    base_path: Option<PathBuf>,
    max_file_size: Option<u64>,
    max_total_size: Option<u64>,
    rotation_period: Option<Duration>,
}

/// Shared finish-fn body: builder-staged fields > [`Inner`].
///
/// Used by both `build()` (which propagates the error) and
/// `build_or_disabled()` (which substitutes [`Inner::Disabled`] on error).
fn assemble(args: AssembleArgs) -> Result<Inner, Dial9ConfigBuilderError> {
    let AssembleArgs {
        tokio_configurators,
        runtime_configurators,
        enabled,
        base_path,
        max_file_size,
        max_total_size,
        rotation_period,
    } = args;

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
            return Err(Dial9ConfigBuilderError::Validation(ValidationError {
                fields: missing,
            }));
        }
    };

    let writer = RotatingWriter::builder()
        .base_path(base_path.clone())
        .max_file_size(max_file_size)
        .max_total_size(max_total_size)
        .maybe_rotation_period(rotation_period)
        .build()
        .map_err(Dial9ConfigBuilderError::Io)?;

    let mut runtime_builder = TracedRuntime::builder().with_trace_path(base_path);
    for configure in runtime_configurators {
        runtime_builder = configure(runtime_builder);
    }

    Ok(Inner::Enabled {
        writer,
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
    /// [`build_or_disabled`](Self::build_or_disabled) can preserve the
    /// configurators on the disabled-fallback variant.
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
    /// Finish into a [`Dial9Config`] that never reports a build error.
    ///
    /// On any [`Dial9ConfigBuilderError`] (validation failure or writer
    /// I/O probe failure) logs an error and returns a [`Dial9Config`]
    /// in its disabled state with the user's `with_tokio` configurators
    /// preserved. The resulting config builds a plain tokio runtime
    /// when handed to [`crate::TracedRuntime::try_new`].
    ///
    /// Missing required fields trigger a [`debug_assert!`] so
    /// misconfigurations surface during development.
    ///
    /// Lenient counterpart to [`build`](Self::build). Use
    /// [`build`](Self::build) instead when you want validation and
    /// writer-I/O failures to surface as
    /// [`Dial9ConfigBuilderError`].
    pub fn build_or_disabled(self) -> Dial9Config {
        let cfgs_for_fallback = self.tokio_configurators.clone();
        match self.build() {
            Ok(cfg) => cfg,
            Err(e) => {
                let is_validation = matches!(e, Dial9ConfigBuilderError::Validation(_));

                debug_assert!(!is_validation, "dial9 config validation failed: {e}");

                let msg = format!(
                    "dial9: telemetry config build failed; falling back to plain tokio runtime: {e}"
                );
                if tracing::dispatcher::has_been_set() {
                    tracing::error!(target: "dial9_telemetry", "{msg}");
                } else {
                    eprintln!("{msg}");
                }

                Dial9Config(Inner::Disabled {
                    tokio_configurators: cfgs_for_fallback,
                })
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;
    use std::sync::atomic::{AtomicUsize, Ordering};

    use crate::TracedRuntime;

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
            Err(Dial9ConfigBuilderError::Validation(v)) => {
                assert_eq!(v.fields(), ["base_path", "max_file_size", "max_total_size"]);
            }
            Ok(_) => panic!("expected Validation error, got Ok"),
            Err(other) => panic!("expected Validation error, got {other:?}"),
        }
    }

    #[test]
    fn missing_some_required_fields_lists_only_missing() {
        match Dial9Config::builder().max_file_size(1024).build() {
            Err(Dial9ConfigBuilderError::Validation(v)) => {
                assert_eq!(v.fields(), ["base_path", "max_total_size"]);
            }
            Ok(_) => panic!("expected Validation error, got Ok"),
            Err(other) => panic!("expected Validation error, got {other:?}"),
        }
    }

    #[test]
    fn explicitly_enabled_still_requires_fields() {
        match Dial9Config::builder().enabled(true).build() {
            Err(Dial9ConfigBuilderError::Validation(_)) => {}
            Ok(_) => panic!("expected Validation error, got Ok"),
            Err(other) => panic!("expected Validation error, got {other:?}"),
        }
    }

    #[test]
    fn strict_build_returns_io_error_for_unwritable_base_path() {
        let result = Dial9Config::builder()
            .base_path(unwritable_base_path())
            .max_file_size(1024)
            .max_total_size(4096)
            .build();
        match result {
            Err(Dial9ConfigBuilderError::Io(_)) => {}
            Ok(_) => panic!("expected Io error, got Ok"),
            Err(other) => panic!("expected Io error, got {other:?}"),
        }
    }

    // ---------------------------------------------------------------
    // build_or_disabled
    // ---------------------------------------------------------------

    #[test]
    #[cfg_attr(debug_assertions, should_panic = "dial9 config validation failed")]
    fn build_or_disabled_from_incomplete_builder_yields_disabled_runtime() {
        let cfg = Dial9Config::builder().build_or_disabled();
        let rt = TracedRuntime::try_new(cfg).expect("fallback runtime should build");
        assert!(
            !rt.guard().is_enabled(),
            "fallback path must yield an inert guard"
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
        let rt = TracedRuntime::try_new(cfg).expect("enabled runtime should build");
        assert!(
            rt.guard().is_enabled(),
            "valid config must keep telemetry enabled"
        );
    }

    #[test]
    fn build_or_disabled_downgrades_on_writer_io_failure() {
        let cfg = Dial9Config::builder()
            .base_path(unwritable_base_path())
            .max_file_size(1024)
            .max_total_size(4096)
            .build_or_disabled();
        let rt =
            TracedRuntime::try_new(cfg).expect("writer I/O failure should downgrade to disabled");
        assert!(
            !rt.guard().is_enabled(),
            "downgrade path must yield an inert guard"
        );
        let v = rt.block_on(async { 42u32 });
        assert_eq!(v, 42);
    }

    #[test]
    fn build_or_disabled_preserves_with_tokio_configurators_on_io_failure() {
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
        let rt = TracedRuntime::try_new(cfg).expect("downgrade should produce a runtime");
        assert!(!rt.guard().is_enabled());
        let calls = counter.load(Ordering::SeqCst);
        assert!(
            calls >= 1,
            "with_tokio configurator must run on the disabled fallback runtime build (was {calls})"
        );
    }

    // ---------------------------------------------------------------
    // Strict-path configurators
    // ---------------------------------------------------------------

    #[test]
    fn strict_build_runs_with_tokio_configurator_on_success() {
        let counter = Arc::new(AtomicUsize::new(0));
        let counter_for_closure = Arc::clone(&counter);
        let cfg = Dial9Config::builder()
            .base_path(tmp_base_path())
            .max_file_size(1024 * 1024)
            .max_total_size(4 * 1024 * 1024)
            .with_tokio(move |b| {
                counter_for_closure.fetch_add(1, Ordering::SeqCst);
                b.worker_threads(2);
            })
            .build()
            .expect("strict build should succeed");
        let _rt = TracedRuntime::try_new(cfg).expect("runtime should build");
        assert_eq!(
            counter.load(Ordering::SeqCst),
            1,
            "with_tokio configurator must run exactly once on strict success path"
        );
    }

    #[test]
    fn strict_build_runs_with_runtime_configurator_on_success() {
        let counter = Arc::new(AtomicUsize::new(0));
        let counter_for_closure = Arc::clone(&counter);
        let cfg = Dial9Config::builder()
            .base_path(tmp_base_path())
            .max_file_size(1024 * 1024)
            .max_total_size(4 * 1024 * 1024)
            .with_runtime(move |r| {
                counter_for_closure.fetch_add(1, Ordering::SeqCst);
                r
            })
            .build()
            .expect("strict build should succeed");
        assert_eq!(
            counter.load(Ordering::SeqCst),
            1,
            "with_runtime configurator must run exactly once during build()"
        );
        let _rt = TracedRuntime::try_new(cfg).expect("runtime should build");
    }

    #[test]
    fn enabled_false_drops_with_runtime_configurators() {
        let counter = Arc::new(AtomicUsize::new(0));
        let counter_for_closure = Arc::clone(&counter);
        let cfg = Dial9Config::builder()
            .enabled(false)
            .with_runtime(move |r| {
                counter_for_closure.fetch_add(1, Ordering::SeqCst);
                r
            })
            .build()
            .expect("disabled build should succeed without required fields");
        let rt = TracedRuntime::try_new(cfg).expect("disabled runtime should build");
        assert!(
            !rt.guard().is_enabled(),
            "disabled config must yield an inert guard"
        );
        assert_eq!(
            counter.load(Ordering::SeqCst),
            0,
            "with_runtime configurators must be ignored when enabled(false)"
        );
    }

    #[test]
    fn multiple_with_tokio_applied_in_declared_order() {
        let order: Arc<std::sync::Mutex<Vec<u32>>> = Arc::new(std::sync::Mutex::new(Vec::new()));
        let order_first = Arc::clone(&order);
        let order_second = Arc::clone(&order);
        let cfg = Dial9Config::builder()
            .base_path(tmp_base_path())
            .max_file_size(1024 * 1024)
            .max_total_size(4 * 1024 * 1024)
            .with_tokio(move |_b| {
                order_first.lock().unwrap().push(1);
            })
            .with_tokio(move |_b| {
                order_second.lock().unwrap().push(2);
            })
            .build()
            .expect("strict build should succeed");
        let _rt = TracedRuntime::try_new(cfg).expect("runtime should build");
        let recorded = order.lock().unwrap().clone();
        assert_eq!(
            recorded,
            vec![1, 2],
            "with_tokio configurators must run in declared order"
        );
    }

    #[test]
    fn dial9_config_builder_error_io_display_and_source_chain() {
        let inner = std::io::Error::other("boom");
        let err = Dial9ConfigBuilderError::Io(inner);
        let display = format!("{err}");
        assert!(
            display.contains("rotating writer:"),
            "Display should label the variant, got: {display}"
        );
        assert!(
            display.contains("boom"),
            "Display should include the inner io::Error message, got: {display}"
        );
        let source = std::error::Error::source(&err);
        assert!(source.is_some(), "source() must return the inner io::Error");
    }
}
