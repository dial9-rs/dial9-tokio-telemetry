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
//! use dial9_perf_self_profile::{PerfSampler, SamplerConfig, SamplingMode, EventSource, Sample};
//!
//! let mut sampler = PerfSampler::start(
//!     SamplerConfig::default()
//!         .event_source(EventSource::SwCpuClock)
//!         .sampling(SamplingMode::FrequencyHz(999)),
//! ).expect("failed to start sampler");
//!
//! // ... do work ...
//!
//! // Drain samples
//! sampler.for_each_sample(|sample: &Sample| {
//!     println!("ip={:#x} callchain={} frames", sample.ip, sample.callchain.len());
//! });
//! ```

pub mod offline_symbolize;
mod sampler;
mod symbolize;
mod sys;
pub mod tracepoint;

pub use offline_symbolize::SymbolTableEntry;
pub use sampler::{EventSource, Sample, SamplerConfig, SamplingMode};
pub use symbolize::{CodeInfo, MapsEntry, SymbolInfo};
pub use symbolize::{parse_proc_maps, read_proc_maps};

// Platform-dispatched re-exports
pub use sys::PerfSampler;
pub use sys::resolve_symbol;

// ctimer fallback status and thread registration
#[cfg(target_os = "linux")]
pub use sys::is_ctimer_active;
#[cfg(not(target_os = "linux"))]
pub fn is_ctimer_active() -> bool {
    false
}

/// Register the calling thread with the active profiling backend.
///
/// No-op unless ctimer fallback is active (perf uses `inherit` instead).
pub fn register_current_thread() -> Result<(), std::io::Error> {
    #[cfg(target_os = "linux")]
    if is_ctimer_active() {
        return crate::sys::fp_profiler::ctimer::register_thread();
    }
    Ok(())
}

/// Unregister the calling thread from the active profiling backend.
///
/// No-op unless ctimer fallback is active.
pub fn unregister_current_thread() {
    #[cfg(target_os = "linux")]
    if is_ctimer_active() {
        crate::sys::fp_profiler::ctimer::unregister_thread();
    }
}

// blazesym-dependent APIs
#[cfg(target_os = "linux")]
pub use sys::{resolve_symbol_with_maps, resolve_symbols_with_maps};
