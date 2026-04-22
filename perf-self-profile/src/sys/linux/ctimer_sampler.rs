//! ctimer-based CPU sampler, provides a fallback for CPU profiling
//! when kernel restrictions make perf inaccessible.
//!
//! Uses per-thread CPU timers to deliver SIGPROF,
//! then unwinds frame pointers via safe_load.

use core::{mem, ptr};
use std::io;
use std::sync::atomic::{AtomicBool, Ordering};

use super::fp_profiler::{
    self, ctimer, sample_buffer,
    unwind::{self, Frame, MAX_FRAMES},
};
use super::gettid;
use super::sampler::SamplerBackend;

use crate::sampler::{Sample, SamplerConfig};

static CTIMER_ACTIVE: AtomicBool = AtomicBool::new(false);

pub fn is_ctimer_active() -> bool {
    CTIMER_ACTIVE.load(Ordering::Relaxed)
}

#[derive(Debug)]
pub(crate) struct CtimerSampler {}

impl CtimerSampler {
    /// Install signal handlers and start the ctimer engine. Does NOT register
    /// any threads. Callers must invoke `track_current_thread` per thread to
    /// begin receiving samples.
    pub fn start(config: &SamplerConfig) -> io::Result<Self> {
        use crate::SamplingMode;

        let freq = match config.sampling {
            SamplingMode::FrequencyHz(hz) => hz.max(1),
            SamplingMode::Period(p) => {
                // Period-based sampling counts kernel events (context switches,
                // tracepoints) which have no userspace equivalent. ctimer can
                // only do frequency-based CPU-time sampling.
                return Err(io::Error::new(
                    io::ErrorKind::Unsupported,
                    format!(
                        "ctimer fallback does not support period-based sampling \
                         (SamplingMode::Period({p})): event-based sources like \
                         context switches require perf to be accessible"
                    ),
                ));
            }
        };
        let interval_ns = 1_000_000_000i64 / (freq as i64);

        unsafe {
            fp_profiler::install_handler().map_err(|e| {
                io::Error::other(format!("failed to install safe_load SIGSEGV handler: {e}"))
            })?;
        }

        unsafe {
            ctimer::start(interval_ns, sigprof_handler)
                .map_err(|e| io::Error::other(format!("failed to start ctimer: {e}")))?;
        }

        CTIMER_ACTIVE.store(true, Ordering::Release);

        Ok(Self {})
    }
}

impl SamplerBackend for CtimerSampler {
    fn track_current_thread(&mut self) -> io::Result<()> {
        ctimer::register_thread()
            .map_err(|e| io::Error::other(format!("ctimer::register_thread failed: {e}")))
    }

    fn stop_tracking_current_thread(&mut self) {
        ctimer::unregister_thread();
    }

    fn has_pending(&self) -> bool {
        sample_buffer::has_pending()
    }

    fn for_each_sample(&mut self, f: &mut dyn FnMut(&Sample)) {
        let dropped = sample_buffer::take_dropped_count();
        if dropped > 0 {
            tracing::debug!("[ctimer] dropped {dropped} samples (buffer full)");
        }

        let mut sample = Sample {
            ip: 0,
            pid: 0,
            tid: 0,
            time: 0,
            cpu: 0,
            period: 0,
            callchain: Vec::with_capacity(MAX_FRAMES),
            raw: None,
        };

        sample_buffer::drain(|raw| {
            let n = (raw.num_frames as usize).min(MAX_FRAMES);
            sample.callchain.clear();
            sample.callchain.extend_from_slice(&raw.frames[..n]);
            sample.ip = sample.callchain.first().copied().unwrap_or(0);
            sample.pid = raw.pid;
            sample.tid = raw.tid;
            sample.time = raw.time;
            sample.cpu = raw.cpu;
            sample.period = raw.period;
            f(&sample);
        });
    }

    fn drain_samples(&mut self) -> Vec<Sample> {
        let mut samples = Vec::new();
        self.for_each_sample(&mut |s| samples.push(s.clone()));
        samples
    }

    fn disable(&self) {
        ctimer::disable();
    }

    fn enable(&self) {
        ctimer::enable();
    }
}

impl Drop for CtimerSampler {
    fn drop(&mut self) {
        ctimer::disable_permanent();
        CTIMER_ACTIVE.store(false, Ordering::Release);
    }
}

// Fired by per-thread CPU timers, must be async-signal-safe.
extern "C" fn sigprof_handler(
    _signo: libc::c_int,
    _info: *mut libc::siginfo_t,
    ucontext: *mut libc::c_void,
) {
    if !ctimer::is_running() {
        if ctimer::is_permanently_stopped()
            && let Some(t) = ctimer::current_thread_timer_id()
        {
            // Self-disarm so the timer doesn't keep firing for the rest of the
            // thread's lifetime. timer_settime is async-signal-safe.
            // SAFETY: `t` is this thread's own live timer_t, zero itimerspec
            // is valid and disarms without deleting.
            unsafe {
                let zero: libc::itimerspec = mem::zeroed();
                libc::timer_settime(t, 0, &zero, ptr::null_mut());
            }
        }
        return;
    }

    // SAFETY: all operations are async-signal-safe.
    unsafe {
        let Some(mut slot) = sample_buffer::claim_slot() else {
            return; // buffer full, sample dropped (counted internally)
        };

        let pid = libc::getpid() as u32;
        let tid = gettid() as u32;

        let mut cpu = 0u32;
        // SAFETY: `getcpu` writes one `u32` through `&mut cpu`, node/cache pointers are null (allowed).
        let ok = libc::syscall(
            libc::SYS_getcpu,
            &mut cpu,
            ptr::null_mut::<libc::c_void>(),
            ptr::null_mut::<libc::c_void>(),
        ) == 0;
        if !ok {
            cpu = 0; // fallback if getcpu fails
        }

        let mut ts: libc::timespec = mem::zeroed();
        libc::clock_gettime(libc::CLOCK_MONOTONIC, &mut ts);
        let time = ts.tv_sec as u64 * 1_000_000_000 + ts.tv_nsec as u64;

        // Account for the number of missed timer expirations.
        let interval_ns = ctimer::interval_ns() as u64;
        let overruns = ctimer::current_thread_timer_id()
            .map(|t| timer_getoverrun(t).max(0) as u64)
            .unwrap_or(0);
        let period = interval_ns.saturating_mul(1 + overruns);

        slot.write(pid, tid, time, cpu, period);

        // Walk the frame-pointer chain.
        let mut frames = [Frame { pc: 0 }; MAX_FRAMES];
        let count = unwind::unwind_from_ucontext(ucontext, &mut frames);

        // Convert Frame PCs to u64 array for the slot.
        let mut frame_pcs = [0u64; MAX_FRAMES];
        for i in 0..count {
            frame_pcs[i] = frames[i].pc as u64;
        }
        slot.write_frames(&frame_pcs, count as u32);

        slot.commit();
    }
}

// timer_getoverrun is POSIX async-signal-safe.
unsafe extern "C" {
    fn timer_getoverrun(timerid: libc::timer_t) -> libc::c_int;
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::SamplingMode;
    use crate::sampler::EventSource;

    #[test]
    fn start_rejects_period_mode() {
        let config = SamplerConfig {
            sampling: SamplingMode::Period(1),
            event_source: EventSource::SwCpuClock,
            include_kernel: false,
        };
        let err = CtimerSampler::start(&config).unwrap_err();
        assert_eq!(err.kind(), io::ErrorKind::Unsupported);
    }
}
