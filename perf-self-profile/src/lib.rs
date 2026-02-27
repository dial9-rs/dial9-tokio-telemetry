//! # perf-self-profile
//!
//! Minimal crate for a program to capture its own perf events with stack traces
//! using Linux `perf_event_open()`. Uses kernel frame-pointer-based stack walking
//! (`PERF_SAMPLE_CALLCHAIN`), so your binary must be compiled with frame pointers:
//!
//! ```toml
//! # Cargo.toml or .cargo/config.toml
//! [profile.release]
//! debug = true
//!
//! # In .cargo/config.toml:
//! [build]
//! rustflags = ["-C", "force-frame-pointers=yes"]
//! ```
//!
//! ## Quick start
//!
//! ```no_run
//! use perf_self_profile::{PerfSampler, SamplerConfig, EventSource, Sample};
//!
//! let mut sampler = PerfSampler::start(SamplerConfig {
//!     frequency_hz: 999,
//!     event_source: EventSource::SwCpuClock,
//!     include_kernel: false,
//! }).expect("failed to start sampler");
//!
//! // ... do work ...
//!
//! // Drain samples
//! sampler.for_each_sample(|sample: &Sample| {
//!     println!("ip={:#x} callchain={} frames", sample.ip, sample.callchain.len());
//! });
//! ```

mod ring_buffer;
mod sampler;
mod symbolize;

pub use sampler::{EventSource, PerfSampler, Sample, SamplerConfig};
pub use symbolize::resolve_symbol;
