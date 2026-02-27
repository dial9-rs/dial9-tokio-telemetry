//! The main sampler: opens perf events, reads samples with callchains.

use std::io;
use std::ptr;

use perf_event_data::Record;
use perf_event_data::endian::Little;
use perf_event_data::parse::{ParseConfig, Parser};
use perf_event_open_sys::bindings::{
    PERF_COUNT_HW_CPU_CYCLES, PERF_COUNT_SW_CONTEXT_SWITCHES, PERF_COUNT_SW_CPU_CLOCK,
    PERF_COUNT_SW_TASK_CLOCK, PERF_FLAG_FD_CLOEXEC, PERF_SAMPLE_CALLCHAIN, PERF_SAMPLE_CPU,
    PERF_SAMPLE_IP, PERF_SAMPLE_PERIOD, PERF_SAMPLE_TID, PERF_SAMPLE_TIME, PERF_TYPE_HARDWARE,
    PERF_TYPE_SOFTWARE, perf_event_attr,
};

use crate::ring_buffer::RingBuffer;

// On x86_64, userspace virtual addresses are below 0x0000_8000_0000_0000.
const USER_ADDR_LIMIT: u64 = 0x0000_8000_0000_0000;

/// Which event source to sample on.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum EventSource {
    /// `PERF_COUNT_HW_CPU_CYCLES` — hardware CPU cycle counter.
    /// Most precise, but may fail in VMs or containers without PMU access.
    HwCpuCycles,
    /// `PERF_COUNT_SW_CPU_CLOCK` — software hrtimer-based CPU clock.
    /// Works everywhere, slightly less precise.
    SwCpuClock,
    /// `PERF_COUNT_SW_TASK_CLOCK` — software task clock (per-thread CPU time).
    SwTaskClock,
    /// `PERF_COUNT_SW_CONTEXT_SWITCHES` — fires on every context switch.
    /// Captures the stack at the moment the thread is descheduled, revealing
    /// what code path led to the thread going off-CPU (e.g. mutex, I/O, preemption).
    SwContextSwitches,
}

/// Configuration for the sampler.
#[derive(Debug, Clone)]
pub struct SamplerConfig {
    /// Sampling frequency in Hz (e.g., 999 or 4000).
    pub frequency_hz: u64,
    /// Which event to sample on.
    pub event_source: EventSource,
    /// Whether to include kernel stack frames.
    /// Requires `perf_event_paranoid` <= 1 (or CAP_PERFMON).
    pub include_kernel: bool,
}

impl Default for SamplerConfig {
    fn default() -> Self {
        SamplerConfig {
            frequency_hz: 999,
            event_source: EventSource::SwCpuClock,
            include_kernel: false,
        }
    }
}

/// A single sample captured from perf events.
#[derive(Debug, Clone)]
pub struct Sample {
    /// Instruction pointer at the time of the sample.
    pub ip: u64,
    /// Process ID.
    pub pid: u32,
    /// Thread ID.
    pub tid: u32,
    /// Timestamp in nanoseconds from `CLOCK_MONOTONIC` (set via `use_clockid`).
    pub time: u64,
    /// CPU the sample was taken on.
    pub cpu: u32,
    /// The actual period for this sample.
    pub period: u64,
    /// Stack frames from the callchain.
    /// First entry is the instruction pointer (leaf), rest are return addresses.
    /// Kernel context markers and hypervisor frames are filtered out.
    pub callchain: Vec<u64>,
}

struct PerfEvent {
    fd: i32,
    ring: RingBuffer,
    /// Thread ID this event is tracking, or 0 for process-wide events.
    tid: i32,
}

impl Drop for PerfEvent {
    fn drop(&mut self) {
        unsafe { perf_event_open_sys::ioctls::DISABLE(self.fd, 0) };
        // RingBuffer::drop handles munmap; closing the fd after munmap is fine on Linux.
        unsafe { libc::close(self.fd) };
    }
}

const PAGE_SIZE: usize = 4096;
const PAGE_COUNT: usize = 16; // power of 2
const MMAP_SIZE: usize = (PAGE_COUNT + 1) * PAGE_SIZE;
const DATA_SIZE: usize = PAGE_COUNT * PAGE_SIZE;

/// Open a perf event fd, mmap the ring buffer, and enable it.
fn open_perf_event(attr: &mut perf_event_attr, pid: i32, cpu: i32) -> io::Result<PerfEvent> {
    let fd = unsafe {
        perf_event_open_sys::perf_event_open(
            attr,
            pid,
            cpu,
            -1,
            PERF_FLAG_FD_CLOEXEC as libc::c_ulong,
        )
    };
    if fd < 0 {
        let err = io::Error::last_os_error();
        if err.kind() == io::ErrorKind::PermissionDenied {
            return Err(io::Error::new(
                io::ErrorKind::PermissionDenied,
                format!(
                    "perf_event_open denied ({}); for context-switch sampling \
                     /proc/sys/kernel/perf_event_paranoid must be <= 1 \
                     (current value can be checked with: \
                     cat /proc/sys/kernel/perf_event_paranoid)",
                    err
                ),
            ));
        }
        return Err(err);
    }

    let base = unsafe {
        libc::mmap(
            ptr::null_mut(),
            MMAP_SIZE,
            libc::PROT_READ | libc::PROT_WRITE,
            libc::MAP_SHARED,
            fd,
            0,
        )
    };

    if base == libc::MAP_FAILED {
        let err = io::Error::last_os_error();
        unsafe { libc::close(fd) };
        return Err(err);
    }

    let ring = unsafe { RingBuffer::new(base as *mut u8, DATA_SIZE as u64, MMAP_SIZE) };

    if unsafe { perf_event_open_sys::ioctls::ENABLE(fd, 0) } < 0 {
        let err = io::Error::last_os_error();
        unsafe { libc::close(fd) };
        return Err(err);
    }

    Ok(PerfEvent { fd, ring, tid: pid })
}

/// A live perf event sampler that profiles the current process.
///
/// Two modes of operation:
///
/// 1. **Process-wide** (`start` / `start_for_pid`): For frequency-based sources,
///    opens one event per CPU with `inherit` to capture all threads automatically.
///    For event-based sources (context switches), opens a single fd with `cpu=-1`.
///
/// 2. **Per-thread** (`new_per_thread` + `add_thread`): Opens one event per thread
///    with `cpu=-1`. Call `add_thread` from an `on_thread_start` callback and
///    `remove_thread` when the thread exits. This works for all event sources
///    including context switches.
pub struct PerfSampler {
    events: Vec<PerfEvent>,
    parse_config: ParseConfig<Little>,
    attr: perf_event_attr,
}

impl PerfSampler {
    /// Start sampling the current process with the given configuration.
    ///
    /// Opens one perf event per online CPU with `inherit` set, so samples
    /// from all threads (including those spawned after this call) are captured.
    pub fn start(config: SamplerConfig) -> io::Result<Self> {
        Self::start_for_pid(0, config) // pid=0 means "this process"
    }

    /// Start sampling a specific process.
    ///
    /// `pid=0` means the current process. `pid=-1` means all processes (requires root).
    pub fn start_for_pid(pid: i32, config: SamplerConfig) -> io::Result<Self> {
        let mut attr = Self::build_attr(&config)?;

        let is_event_based = matches!(config.event_source, EventSource::SwContextSwitches);
        let mut events = Vec::new();

        if is_event_based {
            // Single fd, cpu=-1, pid=target process
            events.push(open_perf_event(&mut attr, pid, -1)?);
        } else {
            // With inherit + sampling, the kernel forbids cpu=-1 for mmap. We open
            // one event per online CPU, each with its own mmap ring buffer.
            let online_cpus = get_online_cpus()?;
            events.reserve(online_cpus.len());
            for &cpu in &online_cpus {
                match open_perf_event(&mut attr, pid, cpu) {
                    Ok(ev) => events.push(ev),
                    Err(e) => {
                        return Err(e);
                    }
                }
            }
        }

        Ok(PerfSampler {
            events,
            parse_config: ParseConfig::from(attr),
            attr,
        })
    }

    /// Create a per-thread sampler with no initial threads.
    ///
    /// Use `track_current_thread` from each thread you want to monitor
    /// (e.g. from an `on_thread_start` callback), and `stop_tracking_current_thread`
    /// when the thread exits.
    ///
    /// This mode works for all event sources including `SwContextSwitches`.
    pub fn new_per_thread(config: SamplerConfig) -> io::Result<Self> {
        let attr = Self::build_attr(&config)?;
        Ok(PerfSampler {
            events: Vec::new(),
            parse_config: ParseConfig::from(attr),
            attr,
        })
    }

    /// Start tracking the calling thread.
    ///
    /// Must be called from the thread you want to monitor. Opens a perf event
    /// fd scoped to this thread's tid with `cpu=-1`.
    pub fn track_current_thread(&mut self) -> io::Result<()> {
        //let tid = unsafe { libc::syscall(libc::SYS_gettid) } as i32;
        let ev = open_perf_event(&mut self.attr, 0, -1)?;
        self.events.push(ev);
        Ok(())
    }

    /// Stop tracking the calling thread.
    ///
    /// Must be called from the same thread that called `track_current_thread`.
    /// Closes the perf event fd and unmaps the ring buffer. Any unread samples
    /// from this thread are lost.
    pub fn stop_tracking_current_thread(&mut self) {
        let tid = unsafe { libc::syscall(libc::SYS_gettid) } as i32;
        if let Some(idx) = self.events.iter().position(|ev| ev.tid == tid) {
            // PerfEvent::drop handles disable + close + munmap
            self.events.swap_remove(idx);
        }
    }

    fn build_attr(config: &SamplerConfig) -> io::Result<perf_event_attr> {
        // Check max sample rate
        if let Ok(contents) = std::fs::read_to_string("/proc/sys/kernel/perf_event_max_sample_rate")
            && let Ok(max_rate) = contents.trim().parse::<u64>()
            && config.frequency_hz > max_rate
        {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                format!(
                    "requested frequency {} exceeds kernel max {} \
                             (see /proc/sys/kernel/perf_event_max_sample_rate)",
                    config.frequency_hz, max_rate
                ),
            ));
        }

        let mut attr = perf_event_attr::default();
        attr.size = std::mem::size_of::<perf_event_attr>() as u32;

        match config.event_source {
            EventSource::HwCpuCycles => {
                attr.type_ = PERF_TYPE_HARDWARE;
                attr.config = PERF_COUNT_HW_CPU_CYCLES as u64;
            }
            EventSource::SwCpuClock => {
                attr.type_ = PERF_TYPE_SOFTWARE;
                attr.config = PERF_COUNT_SW_CPU_CLOCK as u64;
            }
            EventSource::SwTaskClock => {
                attr.type_ = PERF_TYPE_SOFTWARE;
                attr.config = PERF_COUNT_SW_TASK_CLOCK as u64;
            }
            EventSource::SwContextSwitches => {
                attr.type_ = PERF_TYPE_SOFTWARE;
                attr.config = PERF_COUNT_SW_CONTEXT_SWITCHES as u64;
            }
        }

        let is_event_based = matches!(config.event_source, EventSource::SwContextSwitches);

        attr.sample_type = PERF_SAMPLE_IP as u64
            | PERF_SAMPLE_CALLCHAIN as u64
            | PERF_SAMPLE_TID as u64
            | PERF_SAMPLE_TIME as u64
            | PERF_SAMPLE_CPU as u64
            | PERF_SAMPLE_PERIOD as u64;

        // Use CLOCK_MONOTONIC so perf timestamps are in the same clock domain
        // as Rust's `Instant::now()`. Without this, perf defaults to
        // CLOCK_MONOTONIC_RAW which drifts relative to CLOCK_MONOTONIC.
        attr.set_use_clockid(1);
        attr.clockid = libc::CLOCK_MONOTONIC;

        attr.set_disabled(1);
        if is_event_based {
            // Sample every context switch. exclude_kernel must remain 0 since
            // context switches fire in kernel context; kernel callchain frames
            // are filtered at parse time via USER_ADDR_LIMIT.
            attr.sample_period = 1;
            attr.wakeup_events = 1;
            attr.set_sample_id_all(1);
        } else {
            attr.sample_freq = config.frequency_hz;
            attr.set_freq(1);
            attr.set_sample_id_all(1);
            attr.set_inherit(1);
            if !config.include_kernel {
                attr.set_exclude_kernel(1);
                attr.set_exclude_hv(1);
            }
        }

        Ok(attr)
    }

    /// Returns true if there are pending samples to read on any CPU.
    pub fn has_pending(&self) -> bool {
        self.events.iter().any(|ev| ev.ring.has_data())
    }

    /// Drain all pending samples from all CPUs, calling `f` for each one.
    ///
    /// This is non-blocking — if there are no samples, it returns immediately.
    pub fn for_each_sample<F>(&mut self, mut f: F)
    where
        F: FnMut(&Sample),
    {
        let parse_config = self.parse_config.clone();

        for ev in &mut self.events {
            ev.ring.for_each_record(|record| {
                // Assemble header + body into a contiguous slice for perf-event-data.
                // The header is 8 bytes; body may be split across the ring buffer wrap point.
                let mut buf = Vec::with_capacity(record.header.size as usize);
                // SAFETY: perf_event_header is POD; we write it as raw bytes.
                let header_bytes: [u8; 8] = unsafe { std::mem::transmute(record.header) };
                buf.extend_from_slice(&header_bytes);
                match &record.body {
                    crate::ring_buffer::RecordBody::Contiguous(data) => buf.extend_from_slice(data),
                    crate::ring_buffer::RecordBody::Split(a, b) => {
                        buf.extend_from_slice(a);
                        buf.extend_from_slice(b);
                    }
                }

                let mut parser = Parser::new(buf.as_slice(), parse_config.clone());
                let parsed = match parser.parse::<Record>() {
                    Ok(r) => r,
                    Err(_) => return,
                };

                let sample = match parsed {
                    Record::Sample(s) => {
                        let callchain = s
                            .callchain()
                            .unwrap_or(&[])
                            .iter()
                            .copied()
                            .filter(|&a| a < USER_ADDR_LIMIT && a != 0)
                            .collect();
                        Sample {
                            ip: s.ip().unwrap_or(0),
                            pid: s.pid().unwrap_or(0),
                            tid: s.tid().unwrap_or(0),
                            time: s.time().unwrap_or(0),
                            cpu: s.cpu().unwrap_or(0),
                            period: s.period().unwrap_or(0),
                            callchain,
                        }
                    }
                    _ => return,
                };

                f(&sample);
            });
        }
    }

    /// Drain all pending samples into a Vec.
    pub fn drain_samples(&mut self) -> Vec<Sample> {
        let mut samples = Vec::new();
        self.for_each_sample(|s| samples.push(s.clone()));
        samples
    }

    /// Get the raw file descriptors (one per CPU, for use with epoll/poll if needed).
    pub fn fds(&self) -> Vec<i32> {
        self.events.iter().map(|ev| ev.fd).collect()
    }

    /// Returns the number of per-CPU ring buffers that have pending data.
    pub fn active_rings(&self) -> usize {
        self.events.iter().filter(|ev| ev.ring.has_data()).count()
    }

    /// Disable sampling on all CPUs (but keep ring buffers readable).
    pub fn disable(&self) {
        for ev in &self.events {
            unsafe { perf_event_open_sys::ioctls::DISABLE(ev.fd, 0) };
        }
    }

    /// Re-enable sampling after a `disable()`.
    pub fn enable(&self) {
        for ev in &self.events {
            unsafe { perf_event_open_sys::ioctls::ENABLE(ev.fd, 0) };
        }
    }
}

/// Get the list of online CPU indices from /sys/devices/system/cpu/online.
/// Format is like "0-7" or "0-3,5,7-11".
fn get_online_cpus() -> io::Result<Vec<i32>> {
    let content = std::fs::read_to_string("/sys/devices/system/cpu/online")?;
    let mut cpus = Vec::new();
    for part in content.trim().split(',') {
        if let Some((start, end)) = part.split_once('-') {
            let start: i32 = start.parse().map_err(|e| {
                io::Error::new(io::ErrorKind::InvalidData, format!("bad cpu range: {e}"))
            })?;
            let end: i32 = end.parse().map_err(|e| {
                io::Error::new(io::ErrorKind::InvalidData, format!("bad cpu range: {e}"))
            })?;
            cpus.extend(start..=end);
        } else {
            let cpu: i32 = part.parse().map_err(|e| {
                io::Error::new(io::ErrorKind::InvalidData, format!("bad cpu id: {e}"))
            })?;
            cpus.push(cpu);
        }
    }
    Ok(cpus)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_online_cpus_range() {
        // We can't call get_online_cpus with custom input directly since it reads
        // from /sys, but we can verify it works on the current system.
        let cpus = get_online_cpus().expect("should read online cpus");
        assert!(
            !cpus.is_empty(),
            "system should have at least one online CPU"
        );
        // All values should be non-negative
        assert!(cpus.iter().all(|&c| c >= 0));
        // Should be sorted (the kernel format produces sorted ranges)
        for w in cpus.windows(2) {
            assert!(w[0] < w[1], "expected sorted unique CPUs, got {:?}", cpus);
        }
    }
}
