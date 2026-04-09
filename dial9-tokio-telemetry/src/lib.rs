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
/// Core telemetry types, recording, and trace I/O.
pub mod telemetry;
pub(crate) mod traced;
