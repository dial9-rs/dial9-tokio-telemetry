//! [`TelemetryRuntime`] - a tokio runtime paired with its dial9 telemetry guard.

use std::future::Future;

use crate::telemetry::TelemetryGuard;
use crate::{Dial9Config, Dial9ConfigBuilderError};

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
    /// Build from a [`Dial9Config`], panicking with the underlying error if
    /// construction fails. Used by the `#[dial9_tokio_telemetry::main]` macro.
    pub fn from_config(config: Dial9Config) -> Self {
        Self::try_from(config).expect("failed to initialize runtime")
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

impl TryFrom<Dial9Config> for TelemetryRuntime {
    type Error = Dial9ConfigBuilderError;

    fn try_from(config: Dial9Config) -> Result<Self, Self::Error> {
        let (runtime, guard) = config.build()?;
        Ok(Self { runtime, guard })
    }
}
