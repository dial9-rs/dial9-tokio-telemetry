//! The main sampler: opens perf events, reads samples with callchains.

use std::io;
use std::ptr;

use crate::ring_buffer::RingBuffer;
use crate::sys::*;

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
    /// Timestamp (from `CLOCK_MONOTONIC`-ish, in nanoseconds — kernel perf clock).
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

struct PerCpuEvent {
    fd: i32,
    ring: RingBuffer,
}

/// A live perf event sampler that profiles the current process.
///
/// When `inherit` is enabled (the default for `start()`), the kernel requires
/// that each perf event fd is pinned to a specific CPU. We open one event per
/// online CPU so that samples from all threads on all CPUs are captured.
pub struct PerfSampler {
    events: Vec<PerCpuEvent>,
    sample_type: u64,
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
        // Check max sample rate
        if let Ok(contents) = std::fs::read_to_string("/proc/sys/kernel/perf_event_max_sample_rate")
        {
            if let Ok(max_rate) = contents.trim().parse::<u64>() {
                if config.frequency_hz > max_rate {
                    return Err(io::Error::new(
                        io::ErrorKind::InvalidInput,
                        format!(
                            "requested frequency {} exceeds kernel max {} \
                             (see /proc/sys/kernel/perf_event_max_sample_rate)",
                            config.frequency_hz, max_rate
                        ),
                    ));
                }
            }
        }

        let mut attr = PerfEventAttr::zeroed();
        attr.size = std::mem::size_of::<PerfEventAttr>() as u32;

        match config.event_source {
            EventSource::HwCpuCycles => {
                attr.type_ = PERF_TYPE_HARDWARE;
                attr.config = PERF_COUNT_HW_CPU_CYCLES;
            }
            EventSource::SwCpuClock => {
                attr.type_ = PERF_TYPE_SOFTWARE;
                attr.config = PERF_COUNT_SW_CPU_CLOCK;
            }
            EventSource::SwTaskClock => {
                attr.type_ = PERF_TYPE_SOFTWARE;
                attr.config = PERF_COUNT_SW_TASK_CLOCK;
            }
        }

        let sample_type = PERF_SAMPLE_IP
            | PERF_SAMPLE_TID
            | PERF_SAMPLE_TIME
            | PERF_SAMPLE_CALLCHAIN
            | PERF_SAMPLE_CPU
            | PERF_SAMPLE_PERIOD;
        attr.sample_type = sample_type;
        attr.sample_period_or_freq = config.frequency_hz;
        attr.flags = PERF_ATTR_FLAG_FREQ | PERF_ATTR_FLAG_SAMPLE_ID_ALL | PERF_ATTR_FLAG_INHERIT;

        if !config.include_kernel {
            attr.flags |= PERF_ATTR_FLAG_EXCLUDE_KERNEL | PERF_ATTR_FLAG_EXCLUDE_HV;
        }

        // With inherit + sampling, the kernel forbids cpu=-1 for mmap. We open
        // one event per online CPU, each with its own mmap ring buffer.
        let online_cpus = get_online_cpus()?;
        let mut events = Vec::with_capacity(online_cpus.len());

        for &cpu in &online_cpus {
            let fd = perf_event_open(&attr, pid, cpu, -1, PERF_FLAG_FD_CLOEXEC);
            if fd < 0 {
                // Clean up already-opened fds
                for ev in &events {
                    let ev: &PerCpuEvent = ev;
                    unsafe { libc::close(ev.fd) };
                }
                return Err(io::Error::last_os_error());
            }

            let page_count: usize = 16; // power of 2
            let page_size: usize = 4096;
            let mmap_size = (page_count + 1) * page_size;
            let data_size = page_count * page_size;

            // inherit + per-cpu is fine with PROT_READ|PROT_WRITE
            let base = unsafe {
                libc::mmap(
                    ptr::null_mut(),
                    mmap_size,
                    libc::PROT_READ | libc::PROT_WRITE,
                    libc::MAP_SHARED,
                    fd,
                    0,
                )
            };

            if base == libc::MAP_FAILED {
                let err = io::Error::last_os_error();
                unsafe { libc::close(fd) };
                for ev in &events {
                    let ev: &PerCpuEvent = ev;
                    unsafe { libc::close(ev.fd) };
                }
                return Err(io::Error::new(
                    err.kind(),
                    format!(
                        "mmap failed for perf ring buffer on cpu {cpu}: {err}. \
                         You may need to increase perf_event_mlock_kb: \
                         sudo sysctl kernel.perf_event_mlock_kb=2048",
                    ),
                ));
            }

            let ring = unsafe { RingBuffer::new(base as *mut u8, data_size as u64, mmap_size) };

            if perf_event_ioctl(fd, PERF_EVENT_IOC_ENABLE) < 0 {
                let err = io::Error::last_os_error();
                unsafe { libc::close(fd) };
                for ev in &events {
                    let ev: &PerCpuEvent = ev;
                    unsafe { libc::close(ev.fd) };
                }
                return Err(io::Error::new(
                    err.kind(),
                    format!("ioctl PERF_EVENT_IOC_ENABLE failed on cpu {cpu}: {err}"),
                ));
            }

            events.push(PerCpuEvent { fd, ring });
        }

        Ok(PerfSampler {
            events,
            sample_type,
        })
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
        let sample_type = self.sample_type;

        for ev in &mut self.events {
            ev.ring.for_each_record(|record| {
                if record.header.type_ != PERF_RECORD_SAMPLE {
                    return;
                }
                if let Some(sample) = parse_sample(sample_type, &record.body) {
                    f(&sample);
                }
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
            perf_event_ioctl(ev.fd, PERF_EVENT_IOC_DISABLE);
        }
    }

    /// Re-enable sampling after a `disable()`.
    pub fn enable(&self) {
        for ev in &self.events {
            perf_event_ioctl(ev.fd, PERF_EVENT_IOC_ENABLE);
        }
    }
}

impl Drop for PerfSampler {
    fn drop(&mut self) {
        for ev in &mut self.events {
            perf_event_ioctl(ev.fd, PERF_EVENT_IOC_DISABLE);
            unsafe {
                libc::close(ev.fd);
            }
            // RingBuffer drop handles munmap
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

/// Parse a PERF_RECORD_SAMPLE body according to the sample_type we configured.
///
/// The layout of a sample record body is defined by which PERF_SAMPLE_* bits
/// are set. The fields appear in the PERF_RECORD_SAMPLE order documented in
/// perf_event_open(2), which is NOT simply sorted by bit position:
///
/// ```text
///   u64 ip;                          // PERF_SAMPLE_IP
///   u32 pid, tid;                    // PERF_SAMPLE_TID
///   u64 time;                        // PERF_SAMPLE_TIME
///   u32 cpu, res;                    // PERF_SAMPLE_CPU
///   u64 period;                      // PERF_SAMPLE_PERIOD
///   u64 nr;                          // PERF_SAMPLE_CALLCHAIN
///   u64 ips[nr];                     //   (callchain data)
/// ```
fn parse_sample(sample_type: u64, body: &crate::ring_buffer::RecordBody<'_>) -> Option<Sample> {
    let mut offset: usize = 0;
    let body_len = body.len();

    macro_rules! read_u64 {
        () => {{
            if offset + 8 > body_len {
                return None;
            }
            let v = body.read_u64(offset);
            offset += 8;
            v
        }};
    }
    macro_rules! read_u32 {
        () => {{
            if offset + 4 > body_len {
                return None;
            }
            let v = body.read_u32(offset);
            offset += 4;
            v
        }};
    }

    // Fields appear in PERF_RECORD_SAMPLE order (see man page):
    // IDENTIFIER, IP, TID, TIME, ADDR, ID, STREAM_ID, CPU, PERIOD, READ, CALLCHAIN, ...

    let ip = if sample_type & PERF_SAMPLE_IP != 0 {
        read_u64!()
    } else {
        0
    };

    let (pid, tid) = if sample_type & PERF_SAMPLE_TID != 0 {
        let pid = read_u32!();
        let tid = read_u32!();
        (pid, tid)
    } else {
        (0, 0)
    };

    let time = if sample_type & PERF_SAMPLE_TIME != 0 {
        read_u64!()
    } else {
        0
    };

    if sample_type & (1 << 3) != 0 {
        read_u64!(); // PERF_SAMPLE_ADDR
    }

    if sample_type & (1 << 6) != 0 {
        read_u64!(); // PERF_SAMPLE_ID
    }

    // PERF_SAMPLE_STREAM_ID (bit 9)
    if sample_type & (1 << 9) != 0 {
        read_u64!();
    }

    // PERF_SAMPLE_CPU (bit 7) — comes before PERIOD and CALLCHAIN in record layout
    let cpu = if sample_type & PERF_SAMPLE_CPU != 0 {
        let cpu = read_u32!();
        let _res = read_u32!();
        cpu
    } else {
        0
    };

    // PERF_SAMPLE_PERIOD (bit 8) — comes before CALLCHAIN in record layout
    let period = if sample_type & PERF_SAMPLE_PERIOD != 0 {
        read_u64!()
    } else {
        0
    };

    // PERF_SAMPLE_READ (bit 4) — variable length, we never set this

    // PERF_SAMPLE_CALLCHAIN (bit 5) — comes after CPU and PERIOD
    let callchain = if sample_type & PERF_SAMPLE_CALLCHAIN != 0 {
        let nr = read_u64!() as usize;
        if nr > 1024 {
            return None;
        }
        let mut chain = Vec::with_capacity(nr);
        for _ in 0..nr {
            let addr = read_u64!();
            if addr >= PERF_CONTEXT_MAX {
                continue;
            }
            chain.push(addr);
        }
        chain
    } else {
        Vec::new()
    };

    Some(Sample {
        ip,
        pid,
        tid,
        time,
        cpu,
        period,
        callchain,
    })
}
