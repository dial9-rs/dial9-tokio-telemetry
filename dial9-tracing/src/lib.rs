//! Tracing subscriber layer that emits span events into dial9 traces.
//!
//! This crate provides [`Dial9TokioLayer`], a [`tracing_subscriber::Layer`] that
//! records span enter/exit events into the dial9 trace buffer. These events
//! appear alongside poll, park, and wake events in the dial9 viewer, letting you
//! see what happened inside each poll.
//!
//! # Usage
//!
//! ```ignore
//! use dial9_tracing::Dial9TokioLayer;
//! use tracing_subscriber::prelude::*;
//!
//! tracing_subscriber::registry()
//!     .with(Dial9TokioLayer::new())
//!     .init();
//! ```
//!
//! The layer emits events only on threads owned by a dial9-traced runtime.
//! On other threads, span enter/exit is silently skipped.
//!
//! # High-frequency spans
//!
//! Every span enter and exit produces a trace event. If you instrument tight
//! loops, the volume can be large. Use `tracing-subscriber` filters (e.g.,
//! `EnvFilter`, `Targets`) to control which spans reach this layer.
#![warn(
    missing_debug_implementations,
    missing_docs,
    rust_2018_idioms,
    unreachable_pub
)]
#![deny(unused_must_use)]

#[cfg(feature = "tokio")]
mod tokio_layer;

#[cfg(feature = "tokio")]
pub use tokio_layer::Dial9TokioLayer;
