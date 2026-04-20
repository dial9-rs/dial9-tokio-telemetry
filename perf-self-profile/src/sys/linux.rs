mod ctimer_sampler;
pub(crate) mod fp_profiler;
mod offline_symbolize;
mod perf_sampler;
mod ring_buffer;
mod sampler;
mod symbolize;

/// Upper bound of userspace virtual addresses. Addresses at or above this limit
/// are kernel addresses.
///
/// - x86_64: canonical address hole starts at bit 47
/// - aarch64: TTBR0 (user) vs TTBR1 (kernel) selected by bit 63
#[cfg(target_arch = "x86_64")]
pub(crate) const USER_ADDR_LIMIT: u64 = 0x0000_8000_0000_0000;
#[cfg(target_arch = "aarch64")]
pub(crate) const USER_ADDR_LIMIT: u64 = 0x8000_0000_0000_0000;
#[cfg(not(any(target_arch = "x86_64", target_arch = "aarch64")))]
compile_error!("perf-self-profile: USER_ADDR_LIMIT not defined for this architecture");

/// Thread ID of the calling thread.
#[inline]
pub(crate) fn gettid() -> libc::pid_t {
    // SAFETY: gettid has no args, only returns the caller's TID.
    unsafe { libc::syscall(libc::SYS_gettid) as libc::pid_t }
}

pub use ctimer_sampler::is_ctimer_active;
pub(crate) use offline_symbolize::write_symbol_data;
pub use sampler::PerfSampler;
pub use symbolize::{resolve_symbol, resolve_symbol_with_maps, resolve_symbols_with_maps};
