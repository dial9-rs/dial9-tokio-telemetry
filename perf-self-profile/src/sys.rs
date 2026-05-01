#[cfg(target_os = "linux")]
mod linux;
#[cfg(target_os = "linux")]
pub(crate) use linux::fp_profiler;
#[cfg(target_os = "linux")]
pub(crate) use linux::write_symbol_data;
#[cfg(target_os = "linux")]
pub use linux::{
    PerfSampler, is_ctimer_active, resolve_symbol, resolve_symbol_with_maps,
    resolve_symbols_with_maps,
};

#[cfg(not(target_os = "linux"))]
mod unsupported;
#[cfg(not(target_os = "linux"))]
pub(crate) use unsupported::write_symbol_data;
#[cfg(not(target_os = "linux"))]
pub use unsupported::{PerfSampler, resolve_symbol};
