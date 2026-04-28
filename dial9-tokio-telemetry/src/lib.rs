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
/// Core telemetry types, recording, and trace I/O.
pub mod telemetry;
pub(crate) mod traced;

#[cfg(feature = "tracing-layer")]
#[cfg_attr(docsrs, doc(cfg(feature = "tracing-layer")))]
/// Tracing subscriber layer for emitting span events into dial9 traces.
pub mod tracing_layer;

/// Unified configuration for the [`main`] macro.
pub mod config;

pub use dial9_macro::main;
