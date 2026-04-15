#![doc = include_str!("../README.md")]
#![warn(
    missing_debug_implementations,
    missing_docs,
    rust_2018_idioms,
    unreachable_pub
)]
#![deny(unused_must_use, unsafe_op_in_unsafe_fn)]
#![cfg_attr(docsrs, feature(doc_cfg))]

#[cfg(feature = "analysis")]
/// Unstable analysis APIs (feature-gated).
pub mod analysis_unstable;
/// Background worker pipeline for processing sealed trace segments.
pub mod background_task;
pub(crate) mod metrics;
pub(crate) mod rate_limit;
/// Core telemetry types, recording, and trace I/O.
pub mod telemetry;
pub(crate) mod traced;

/// Unified configuration for the [`main`] macro.
pub mod config;

/// Instrument an async main function with dial9 telemetry.
///
/// This macro wraps your function body in a spawned task so that poll events
/// are recorded. Without it, code running directly in `runtime.block_on(...)`
/// is invisible to the telemetry hooks. To spawn instrumented sub-tasks from
/// inside the body, call [`telemetry::TelemetryHandle::current`] — the handle
/// is installed on every runtime-owned thread by `on_thread_start`.
///
/// See [`config::Dial9Config`] for configuration options and the
/// [crate-level docs](crate) for a full example.
///
/// # Usage
///
/// ```rust,ignore
/// use dial9_tokio_telemetry::{main, config::Dial9Config, telemetry::TelemetryHandle};
///
/// fn my_config() -> Dial9Config {
///     Dial9Config::new("/tmp/trace.bin", 1024 * 1024, 16 * 1024 * 1024)
/// }
///
/// #[dial9_tokio_telemetry::main(config = my_config)]
/// async fn main() {
///     let handle = TelemetryHandle::current();
///     handle
///         .spawn(async { /* instrumented */ })
///         .await
///         .unwrap();
/// }
/// ```
pub use dial9_macro::main;
