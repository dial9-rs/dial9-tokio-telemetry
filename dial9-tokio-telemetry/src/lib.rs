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

#[cfg(feature = "tracing-layer")]
#[cfg_attr(docsrs, doc(cfg(feature = "tracing-layer")))]
/// Tracing subscriber layer for emitting span events into dial9 traces.
pub mod tracing_layer;

/// Deprecated configuration, kept for back-compat. Prefer [`current_config`]
/// (re-exported at the crate root as [`Dial9Config`] / [`Dial9ConfigBuilder`]).
#[path = "legacy_config.rs"]
pub mod config;

#[path = "config.rs"]
mod current_config;

pub use current_config::{Dial9Config, Dial9ConfigBuilder, Dial9ConfigBuilderError, MissingFields};
pub use dial9_macro::main;
