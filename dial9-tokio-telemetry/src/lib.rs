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
pub(crate) mod primitives;
pub(crate) mod rate_limit;
#[cfg(feature = "taskdump")]
pub(crate) mod task_dumped;
/// Core telemetry types, recording, and trace I/O.
pub mod telemetry;
pub(crate) mod traced;

#[cfg(feature = "tracing-layer")]
#[cfg_attr(docsrs, doc(cfg(feature = "tracing-layer")))]
/// Tracing subscriber layer for emitting span events into dial9 traces.
pub mod tracing_layer;

/// Original positional-argument config API for the
/// `#[dial9_tokio_telemetry::main]` macro. The fluent builder re-exported
/// at the crate root (see [`Dial9Config::builder`]) is a more ergonomic alternative.
/// We encourage you to switch to [`Dial9Config::builder`].
#[path = "legacy_config.rs"]
pub mod config;

#[path = "config.rs"]
mod current_config;

pub use current_config::{
    Dial9Config, Dial9ConfigBuilder, Dial9ConfigBuilderError, ValidationError,
};
pub use dial9_macro::main;
pub use telemetry::{TelemetryRuntimeError, TracedRuntime, spawn};
