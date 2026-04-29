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
//! - `TryFrom<`[`crate::Dial9Config`]`>` — strict. Returns
//!   [`TelemetryRuntimeError`].
//! - `TryFrom<`[`crate::Dial9ConfigFallback`]`>` — lenient. Returns
//!   [`std::io::Error`]; only the tokio builder's own error can escape.
//! - `TryFrom<`[`crate::config::Dial9Config`]`>` — bridge for the
//!   deprecated positional config. Returns [`std::io::Error`].

use std::future::Future;

use crate::Dial9Config;
use crate::current_config::{Dial9ConfigFallback, Inner, materialize_tokio_builder};
use crate::telemetry::TelemetryGuard;
use crate::telemetry::writer::RotatingWriter;

/// Errors produced while constructing a [`TelemetryRuntime`] from a
/// strict [`Dial9Config`].
///
/// The lenient counterpart [`Dial9ConfigFallback`] narrows its
/// `TryFrom::Error` to [`std::io::Error`]; this enum is the wider error
/// shape returned only from the strict [`Dial9Config`] conversion.
#[derive(Debug)]
#[non_exhaustive]
pub enum TelemetryRuntimeError {
    /// Failure from [`tokio::runtime::Builder::build`].
    TokioRuntimeBuilder(std::io::Error),
    /// Failure from [`RotatingWriter`] construction.
    RotatingWriter(std::io::Error),
    /// Failure from telemetry core setup (traced runtime + background worker).
    TelemetryCore(std::io::Error),
}

impl std::fmt::Display for TelemetryRuntimeError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            TelemetryRuntimeError::TokioRuntimeBuilder(e) => {
                write!(f, "tokio runtime builder: {e}")
            }
            TelemetryRuntimeError::RotatingWriter(e) => write!(f, "rotating writer: {e}"),
            TelemetryRuntimeError::TelemetryCore(e) => write!(f, "telemetry core: {e}"),
        }
    }
}

impl std::error::Error for TelemetryRuntimeError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            TelemetryRuntimeError::TokioRuntimeBuilder(e)
            | TelemetryRuntimeError::RotatingWriter(e)
            | TelemetryRuntimeError::TelemetryCore(e) => Some(e),
        }
    }
}

/// A tokio runtime paired with its (optional) dial9 telemetry guard.
///
/// The guard, when present, must outlive the runtime so traces are flushed on
/// drop - keeping both inside one struct enforces that ordering at the type
/// level. Construct one via [`TelemetryRuntime::try_from`] from a
/// [`Dial9Config`] (strict) or [`Dial9ConfigFallback`] (lenient).
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
    /// Generic over any input that converts into a [`TelemetryRuntime`],
    /// so it accepts strict ([`Dial9Config`]), lenient
    /// ([`Dial9ConfigFallback`]), and the deprecated positional
    /// [`crate::config::Dial9Config`] configs transparently. The
    /// generic shape is what keeps the macro source-compatible across
    /// these three input types.
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
/// Shared body of both `TryFrom<Dial9Config>` (where every error
/// propagates verbatim) and `TryFrom<Dial9ConfigFallback>` (which catches
/// `RotatingWriter` and `TelemetryCore` errors and replays the
/// configurators into a plain tokio runtime).
fn try_assemble(
    inner: Inner,
) -> Result<(tokio::runtime::Runtime, Option<TelemetryGuard>), TelemetryRuntimeError> {
    match inner {
        Inner::Enabled {
            base_path,
            max_file_size,
            max_total_size,
            rotation_period,
            tokio_configurators,
            runtime_builder,
        } => {
            let writer = RotatingWriter::builder()
                .base_path(base_path)
                .max_file_size(max_file_size)
                .max_total_size(max_total_size)
                .maybe_rotation_period(rotation_period)
                .build()
                .map_err(TelemetryRuntimeError::RotatingWriter)?;
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

/// Lenient construction path: `RotatingWriter` and telemetry-core I/O
/// failures silently cascade to a plain tokio runtime built from the
/// user's `with_tokio` configurators (replayed onto a fresh
/// [`tokio::runtime::Builder`]). Only the tokio builder's own
/// [`std::io::Error`] can still escape — there is no fallback for
/// "cannot build a tokio runtime at all".
impl TryFrom<Dial9ConfigFallback> for TelemetryRuntime {
    type Error = std::io::Error;

    fn try_from(config: Dial9ConfigFallback) -> Result<Self, Self::Error> {
        // Snapshot the configurators up-front: try_assemble consumes the
        // Inner::Enabled variant on the primary attempt, and the cascade
        // path needs to replay them onto a fresh tokio::runtime::Builder.
        let configurators_for_fallback = match &config.0 {
            Inner::Enabled {
                tokio_configurators,
                ..
            }
            | Inner::Disabled {
                tokio_configurators,
            } => tokio_configurators.clone(),
        };

        match try_assemble(config.0) {
            Ok((runtime, guard)) => Ok(Self { runtime, guard }),
            // Tokio builder failure has nowhere to fall back to — propagate.
            Err(TelemetryRuntimeError::TokioRuntimeBuilder(e)) => Err(e),
            // Cascade RotatingWriter / TelemetryCore failures to a plain
            // tokio runtime built from the replayed configurators.
            Err(TelemetryRuntimeError::RotatingWriter(_))
            | Err(TelemetryRuntimeError::TelemetryCore(_)) => {
                let runtime = materialize_tokio_builder(&configurators_for_fallback).build()?;
                Ok(Self {
                    runtime,
                    guard: None,
                })
            }
        }
    }
}
