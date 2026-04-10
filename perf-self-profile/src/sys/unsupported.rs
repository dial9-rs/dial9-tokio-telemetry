use std::io;

use crate::sampler::{Sample, SamplerConfig};
use crate::symbolize::SymbolInfo;

fn unsupported<T>() -> io::Result<T> {
    Err(io::Error::new(
        io::ErrorKind::Unsupported,
        "PerfSampler is only available on Linux",
    ))
}

/// Stub `PerfSampler` for non-Linux platforms.
///
/// All constructors return [`io::ErrorKind::Unsupported`].
pub struct PerfSampler {
    _private: (),
}

impl PerfSampler {
    pub fn start(_config: SamplerConfig) -> io::Result<Self> {
        unsupported()
    }

    pub fn new_per_thread(_config: SamplerConfig) -> io::Result<Self> {
        unsupported()
    }

    pub fn track_current_thread(&mut self) -> io::Result<()> {
        unsupported()
    }

    pub fn stop_tracking_current_thread(&mut self) {}

    pub fn for_each_sample<F>(&mut self, _f: F)
    where
        F: FnMut(&Sample),
    {
    }

    pub fn drain_samples(&mut self) -> Vec<Sample> {
        Vec::new()
    }

    pub fn disable(&self) {}

    pub fn enable(&self) {}
}

pub(crate) fn write_symbol_data(
    _decoder: dial9_trace_format::decoder::Decoder<'_>,
    _addresses: &std::collections::BTreeSet<u64>,
    _maps: &[crate::MapsEntry],
    _output: &mut impl std::io::Write,
) -> std::io::Result<()> {
    Ok(())
}

pub fn resolve_symbol(_addr: u64) -> SymbolInfo {
    SymbolInfo {
        name: None,
        base_addr: 0,
        code_info: None,
        offset: 0,
    }
}
