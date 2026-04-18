//! Public `PerfSampler`, dispatch layer over perf and ctimer backends.
//!
//! Attempts to use `perf_event_open` first. If blocked (e.g. kernel restrictions),
//! falls back to ctimer-based sampling with userspace frame-pointer unwinding.

use std::io;

use super::ctimer_sampler::CtimerSampler;
use super::perf_sampler::PerfSamplerImpl;
use crate::sampler::{Sample, SamplerConfig};

pub(super) trait SamplerBackend: Send {
    fn track_current_thread(&mut self) -> io::Result<()>;
    fn stop_tracking_current_thread(&mut self);
    fn has_pending(&self) -> bool;
    fn for_each_sample(&mut self, f: &mut dyn FnMut(&Sample));
    fn drain_samples(&mut self) -> Vec<Sample>;
    fn disable(&self);
    fn enable(&self);
}

/// CPU sampler dispatching to perf_event_open or ctimer (fallback).
pub struct PerfSampler {
    inner: Box<dyn SamplerBackend>,
}

/// Returns `true` if the error indicates `perf_event_open` is blocked by
/// the kernel (seccomp, perf_event_paranoid, missing syscall).
pub(crate) fn is_perf_blocked(err: &io::Error) -> bool {
    matches!(
        err.raw_os_error(),
        Some(libc::EACCES) | Some(libc::EPERM) | Some(libc::ENOSYS) | Some(libc::EOPNOTSUPP)
    )
}

impl PerfSampler {
    /// Start sampling the current process.
    pub fn start(config: SamplerConfig) -> io::Result<Self> {
        Self::start_for_pid(0, config)
    }

    /// Start sampling a specific process.
    ///
    /// `pid=0` means the current process. `pid=-1` means all processes (requires root).
    pub fn start_for_pid(pid: i32, config: SamplerConfig) -> io::Result<Self> {
        use crate::SamplingMode;

        if std::env::var_os("DIAL9_FORCE_CTIMER").is_some() {
            match config.sampling {
                SamplingMode::FrequencyHz(_) => {
                    tracing::info!("DIAL9_FORCE_CTIMER set, using ctimer-based CPU profiling");
                    return Self::with_ctimer_process_wide(&config);
                }
                SamplingMode::Period(_) => {
                    tracing::warn!(
                        "DIAL9_FORCE_CTIMER set but ignored: ctimer only supports \
                         frequency-based sampling, not period-based"
                    );
                }
            }
        }

        match PerfSamplerImpl::start_for_pid(pid, &config) {
            Ok(perf) => Ok(Self {
                inner: Box::new(perf),
            }),
            Err(e) if is_perf_blocked(&e) => {
                if pid != 0 && pid != -1 {
                    tracing::warn!(
                        "ctimer fallback cannot profile pid {pid}, \
                         will sample current process only"
                    );
                }
                tracing::warn!(
                    "perf_event_open failed ({e}), falling back to ctimer-based \
                     CPU profiling (userspace frame-pointer unwinding)"
                );
                Self::with_ctimer_process_wide(&config)
            }
            Err(e) => Err(e),
        }
    }

    /// Create a per-thread sampler with no initial threads.
    ///
    /// Falls back to ctimer for frequency-based sampling when perf is blocked.
    /// Event-based sampling (context switches, tracepoints) requires
    /// `perf_event_open` and cannot fall back — returns an error if blocked.
    pub fn new_per_thread(config: SamplerConfig) -> io::Result<Self> {
        use crate::SamplingMode;

        if std::env::var_os("DIAL9_FORCE_CTIMER").is_some() {
            match config.sampling {
                SamplingMode::FrequencyHz(_) => {
                    tracing::info!("DIAL9_FORCE_CTIMER set, using ctimer-based profiling");
                    return Self::with_ctimer(&config);
                }
                SamplingMode::Period(_) => {
                    tracing::warn!(
                        "DIAL9_FORCE_CTIMER set but ignored: ctimer only supports \
                         frequency-based sampling, not period-based"
                    );
                }
            }
        }

        match PerfSamplerImpl::new_per_thread(&config) {
            Ok(perf) => Ok(Self {
                inner: Box::new(perf),
            }),
            Err(e) if is_perf_blocked(&e) => match config.sampling {
                SamplingMode::FrequencyHz(_) => {
                    tracing::warn!(
                        "perf_event_open failed ({e}), falling back to ctimer-based \
                         profiling for per-thread mode"
                    );
                    Self::with_ctimer(&config)
                }
                SamplingMode::Period(_) => Err(io::Error::new(
                    io::ErrorKind::Unsupported,
                    format!(
                        "perf_event_open blocked ({e}) and event-based sampling \
                         has no userspace fallback (requires kernel support)"
                    ),
                )),
            },
            Err(e) => Err(e),
        }
    }

    /// Create ctimer sampler without registering any threads (per-thread mode).
    fn with_ctimer(config: &SamplerConfig) -> io::Result<Self> {
        Ok(Self {
            inner: Box::new(CtimerSampler::start(config)?),
        })
    }

    /// Create ctimer sampler and register the calling thread (process-wide mode).
    fn with_ctimer_process_wide(config: &SamplerConfig) -> io::Result<Self> {
        let mut sampler = Self::with_ctimer(config)?;
        sampler.track_current_thread()?;
        Ok(sampler)
    }

    pub fn track_current_thread(&mut self) -> io::Result<()> {
        self.inner.track_current_thread()
    }

    pub fn stop_tracking_current_thread(&mut self) {
        self.inner.stop_tracking_current_thread()
    }

    pub fn has_pending(&self) -> bool {
        self.inner.has_pending()
    }

    pub fn for_each_sample<F: FnMut(&Sample)>(&mut self, mut f: F) {
        self.inner.for_each_sample(&mut f)
    }

    pub fn drain_samples(&mut self) -> Vec<Sample> {
        self.inner.drain_samples()
    }

    pub fn disable(&self) {
        self.inner.disable()
    }

    pub fn enable(&self) {
        self.inner.enable()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn is_perf_blocked_recognizes_blocked_errors() {
        for errno in [libc::EACCES, libc::EPERM, libc::ENOSYS, libc::EOPNOTSUPP] {
            let err = io::Error::from_raw_os_error(errno);
            assert!(
                is_perf_blocked(&err),
                "expected is_perf_blocked=true for errno {errno}"
            );
        }
    }

    #[test]
    fn is_perf_blocked_ignores_other_errors() {
        for errno in [libc::EINVAL, libc::ENOENT, libc::EBADF] {
            let err = io::Error::from_raw_os_error(errno);
            assert!(
                !is_perf_blocked(&err),
                "expected is_perf_blocked=false for errno {errno}"
            );
        }
    }
}
