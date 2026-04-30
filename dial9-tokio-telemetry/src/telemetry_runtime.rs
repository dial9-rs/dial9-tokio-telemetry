//! [`TelemetryRuntime`] — a tokio runtime paired with its dial9 telemetry guard.
//!
//! This is the construction glue that the
//! `#[dial9_tokio_telemetry::main]` macro expands into. It is also a
//! standalone public type for callers who want the same
//! macro-equivalent setup without the attribute (e.g. tests, custom
//! `main` functions, or libraries that build their own runtime).
//!
//! Construct one via [`TelemetryRuntime::from_config`] (panics on
//! failure, used by the macro) or directly via the [`TryFrom`] impls:
//!
//! - `TryFrom<`[`crate::Dial9Config`]`>` — primary path. Returns
//!   [`TelemetryRuntimeError`].
//! - `TryFrom<`[`crate::config::Dial9Config`]`>` — bridge for the
//!   deprecated positional config. Returns [`std::io::Error`].

use std::future::Future;

use crate::Dial9Config;
use crate::current_config::{Inner, materialize_tokio_builder};
use crate::telemetry::TelemetryGuard;

/// Errors produced while constructing a [`TelemetryRuntime`] from a
/// [`Dial9Config`].
///
/// Writer-transport I/O has already been validated by
/// [`crate::Dial9ConfigBuilder::build`], so the only remaining failure
/// modes here come from the tokio runtime builder and the telemetry
/// background worker startup.
#[derive(Debug)]
#[non_exhaustive]
pub enum TelemetryRuntimeError {
    /// Failure from [`tokio::runtime::Builder::build`].
    TokioRuntimeBuilder(std::io::Error),
    /// Failure from telemetry core setup (traced runtime + background worker).
    TelemetryCore(std::io::Error),
}

impl std::fmt::Display for TelemetryRuntimeError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            TelemetryRuntimeError::TokioRuntimeBuilder(e) => {
                write!(f, "tokio runtime builder: {e}")
            }
            TelemetryRuntimeError::TelemetryCore(e) => write!(f, "telemetry core: {e}"),
        }
    }
}

impl std::error::Error for TelemetryRuntimeError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            TelemetryRuntimeError::TokioRuntimeBuilder(e)
            | TelemetryRuntimeError::TelemetryCore(e) => Some(e),
        }
    }
}

/// A tokio runtime paired with its (optional) dial9 telemetry guard.
///
/// The guard, when present, must outlive the runtime so traces are flushed on
/// drop - keeping both inside one struct enforces that ordering at the type
/// level. Construct one via [`TelemetryRuntime::try_from`] from a
/// [`Dial9Config`].
#[derive(Debug)]
pub struct TelemetryRuntime {
    runtime: tokio::runtime::Runtime,
    guard: Option<TelemetryGuard>,
}

impl TelemetryRuntime {
    /// Build from a config, panicking with the underlying error if
    /// construction fails. Used by the `#[dial9_tokio_telemetry::main]`
    /// macro.
    ///
    /// You will also reach for this directly when the macro doesn't fit —
    /// e.g. when an application owns multiple tokio runtimes, when you
    /// need to control runtime lifetime explicitly, or when you want to
    /// drive graceful shutdown via [`guard()`](Self::guard) before the
    /// runtime drops.
    ///
    /// Generic over any input that converts into a [`TelemetryRuntime`],
    /// so it accepts both [`Dial9Config`] and the deprecated positional
    /// [`crate::config::Dial9Config`] transparently. The generic shape
    /// is what keeps the macro source-compatible across these input
    /// types.
    ///
    /// # Panics
    ///
    /// Panics if the underlying conversion fails — i.e. if the tokio
    /// runtime cannot be built or the telemetry background worker fails
    /// to start. Writer-transport I/O has already been validated by
    /// [`crate::Dial9ConfigBuilder::build`], so it cannot fail here.
    ///
    /// For fallible construction, use the [`TryFrom`] impl directly:
    ///
    /// ```no_run
    /// # use dial9_tokio_telemetry::{Dial9Config, TelemetryRuntime};
    /// let cfg = Dial9Config::builder()
    ///     .base_path("trace.bin")
    ///     .max_file_size(64 * 1024 * 1024)
    ///     .max_total_size(1024 * 1024 * 1024)
    ///     .build()?;
    /// let rt = TelemetryRuntime::try_from(cfg)?;
    /// # Ok::<(), Box<dyn std::error::Error>>(())
    /// ```
    pub fn from_config<C>(config: C) -> Self
    where
        C: TryInto<TelemetryRuntime>,
        <C as TryInto<TelemetryRuntime>>::Error: std::fmt::Display,
    {
        config
            .try_into()
            .unwrap_or_else(|e| panic!("failed to initialize runtime: {e}"))
    }

    /// Borrow the underlying tokio runtime.
    pub fn runtime(&self) -> &tokio::runtime::Runtime {
        &self.runtime
    }

    /// Borrow the telemetry guard, if telemetry was enabled.
    pub fn guard(&self) -> Option<&TelemetryGuard> {
        self.guard.as_ref()
    }

    /// Run `fut` to completion on the runtime.
    ///
    /// When telemetry is enabled, the future is spawned through the
    /// [`TelemetryHandle`](crate::telemetry::TelemetryHandle) so its poll and
    /// wake events are recorded. When telemetry is disabled, this is just a
    /// passthrough to [`tokio::runtime::Runtime::block_on`].
    pub fn block_on<F>(&self, fut: F) -> F::Output
    where
        F: Future + Send + 'static,
        F::Output: Send + 'static,
    {
        if let Some(guard) = &self.guard {
            let handle = guard.handle();
            self.runtime.block_on(async move {
                match handle.spawn(fut).await {
                    Ok(output) => output,
                    Err(err) if err.is_panic() => std::panic::resume_unwind(err.into_panic()),
                    Err(_) => unreachable!("task cannot be cancelled inside block_on"),
                }
            })
        } else {
            self.runtime.block_on(fut)
        }
    }
}

/// Drive an [`Inner`] to a tokio runtime + (optional) guard.
///
/// `Inner::Enabled` already carries a built [`RotatingWriter`] (see
/// [`crate::config`]), so this only needs to materialize the tokio
/// builder and hand both off to
/// [`TracedRuntimeBuilder::build_and_start`].
fn try_assemble(
    inner: Inner,
) -> Result<(tokio::runtime::Runtime, Option<TelemetryGuard>), TelemetryRuntimeError> {
    match inner {
        Inner::Enabled {
            writer,
            tokio_configurators,
            runtime_builder,
        } => {
            let tokio_builder = materialize_tokio_builder(&tokio_configurators);
            let (runtime, guard) = runtime_builder
                .build_and_start(tokio_builder, writer)
                .map_err(TelemetryRuntimeError::TelemetryCore)?;
            Ok((runtime, Some(guard)))
        }
        Inner::Disabled {
            tokio_configurators,
        } => {
            let runtime = materialize_tokio_builder(&tokio_configurators)
                .build()
                .map_err(TelemetryRuntimeError::TokioRuntimeBuilder)?;
            Ok((runtime, None))
        }
    }
}

impl TryFrom<Dial9Config> for TelemetryRuntime {
    type Error = TelemetryRuntimeError;

    fn try_from(config: Dial9Config) -> Result<Self, Self::Error> {
        let (runtime, guard) = try_assemble(config.0)?;
        Ok(Self { runtime, guard })
    }
}

/// Bridge for the deprecated positional config API at
/// [`crate::config::Dial9Config`] so that it remains compatible with
/// [`TelemetryRuntime::from_config`] (and therefore the
/// `#[dial9_tokio_telemetry::main]` macro).
impl TryFrom<crate::config::Dial9Config> for TelemetryRuntime {
    type Error = std::io::Error;

    fn try_from(config: crate::config::Dial9Config) -> Result<Self, Self::Error> {
        let (runtime, guard) = config.build()?;
        Ok(Self { runtime, guard })
    }
}

#[cfg(test)]
mod tests {
    use std::path::PathBuf;

    use super::*;

    fn tmp_base_path() -> PathBuf {
        let dir = tempfile::tempdir().expect("tempdir");
        // Leak the TempDir so it isn't deleted while the test runs.
        let path = dir.path().join("trace.bin");
        std::mem::forget(dir);
        path
    }

    #[test]
    fn direct_block_on_enabled_path_returns_value_and_exposes_guard() {
        let cfg = Dial9Config::builder()
            .base_path(tmp_base_path())
            .max_file_size(1024 * 1024)
            .max_total_size(4 * 1024 * 1024)
            .build()
            .expect("strict build should succeed");
        let rt = TelemetryRuntime::try_from(cfg).expect("runtime should build");
        assert!(rt.guard().is_some(), "enabled config must install a guard");
        // Smoke-test the runtime accessor — exists and is usable.
        let _ = rt.runtime().handle();
        let value = rt.block_on(async { 5u32 });
        assert_eq!(value, 5);
    }

    #[test]
    fn direct_block_on_disabled_path_returns_value_no_guard() {
        let cfg = Dial9Config::builder()
            .enabled(false)
            .build()
            .expect("disabled build should succeed");
        let rt = TelemetryRuntime::try_from(cfg).expect("disabled runtime should build");
        assert!(
            rt.guard().is_none(),
            "disabled config must not install a guard"
        );
        let value = rt.block_on(async { 11u32 });
        assert_eq!(value, 11);
    }

    #[test]
    fn telemetry_runtime_error_display_and_source_chain() {
        let inner = std::io::Error::other("boom");
        let err = TelemetryRuntimeError::TelemetryCore(inner);
        let display = format!("{err}");
        assert!(
            display.contains("telemetry core:"),
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
