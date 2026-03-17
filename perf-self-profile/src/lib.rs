//! # perf-self-profile
//!
//! Minimal crate for a program to capture its own perf events with stack traces
//! using Linux `perf_event_open()`.
//!
//! This crate relies on `perf_event_paranoid <= 2`.
//!
//! Uses kernel frame-pointer-based stack walking
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
//! use dial9_perf_self_profile::{PerfSampler, SamplerConfig, EventSource, Sample};
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
pub mod tracepoint;

/// Upper bound of userspace virtual addresses. Addresses at or above this limit
/// are kernel addresses.
///
/// - x86_64: canonical address hole starts at bit 47
/// - aarch64: TTBR0 (user) vs TTBR1 (kernel) selected by bit 63
#[cfg(target_arch = "x86_64")]
pub const USER_ADDR_LIMIT: u64 = 0x0000_8000_0000_0000;
#[cfg(target_arch = "aarch64")]
pub const USER_ADDR_LIMIT: u64 = 0x8000_0000_0000_0000;
#[cfg(not(any(target_arch = "x86_64", target_arch = "aarch64")))]
compile_error!("perf-self-profile: USER_ADDR_LIMIT not defined for this architecture");

pub use sampler::{EventSource, PerfSampler, Sample, SamplerConfig};
pub use symbolize::resolve_symbol;
