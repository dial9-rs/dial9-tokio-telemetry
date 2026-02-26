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

struct PerfEvent {
    fd: i32,
    ring: RingBuffer,
    /// Thread ID this event is tracking, or 0 for process-wide events.
    tid: i32,
}

impl Drop for PerfEvent {
    fn drop(&mut self) {
        perf_event_ioctl(self.fd, PERF_EVENT_IOC_DISABLE);
        // RingBuffer::drop handles munmap; closing the fd after munmap is fine on Linux.
        unsafe { libc::close(self.fd) };
    }
}

const PAGE_SIZE: usize = 4096;
const PAGE_COUNT: usize = 16; // power of 2
const MMAP_SIZE: usize = (PAGE_COUNT + 1) * PAGE_SIZE;
const DATA_SIZE: usize = PAGE_COUNT * PAGE_SIZE;

/// Open a perf event fd, mmap the ring buffer, and enable it.
fn open_perf_event(attr: &PerfEventAttr, pid: i32, cpu: i32) -> io::Result<PerfEvent> {
    let fd = perf_event_open(attr, pid, cpu, -1, PERF_FLAG_FD_CLOEXEC);
    if fd < 0 {
        return Err(io::Error::last_os_error());
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

    if perf_event_ioctl(fd, PERF_EVENT_IOC_ENABLE) < 0 {
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
    sample_type: u64,
    /// Stored attr for per-thread tracking.
    attr: PerfEventAttr,
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
        let attr = Self::build_attr(&config)?;

        let is_event_based = matches!(config.event_source, EventSource::SwContextSwitches);
        let mut events = Vec::new();

        if is_event_based {
            // Single fd, cpu=-1, pid=target process
            events.push(open_perf_event(&attr, pid, -1)?);
        } else {
            // With inherit + sampling, the kernel forbids cpu=-1 for mmap. We open
            // one event per online CPU, each with its own mmap ring buffer.
            let online_cpus = get_online_cpus()?;
            events.reserve(online_cpus.len());
            for &cpu in &online_cpus {
                match open_perf_event(&attr, pid, cpu) {
                    Ok(ev) => events.push(ev),
                    Err(e) => {
                        drop(events);
                        return Err(e);
                    }
                }
            }
        }

        Ok(PerfSampler {
            events,
            sample_type: attr.sample_type,
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
            sample_type: attr.sample_type,
            attr,
        })
    }

    /// Start tracking the calling thread.
    ///
    /// Must be called from the thread you want to monitor. Opens a perf event
    /// fd scoped to this thread's tid with `cpu=-1`.
    pub fn track_current_thread(&mut self) -> io::Result<()> {
        let tid = unsafe { libc::syscall(libc::SYS_gettid) } as i32;
        let ev = open_perf_event(&self.attr, tid, -1)?;
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

    fn build_attr(config: &SamplerConfig) -> io::Result<PerfEventAttr> {
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
            EventSource::SwContextSwitches => {
                attr.type_ = PERF_TYPE_SOFTWARE;
                attr.config = PERF_COUNT_SW_CONTEXT_SWITCHES;
            }
        }

        attr.sample_type = PERF_SAMPLE_IP
            | PERF_SAMPLE_CALLCHAIN
            | PERF_SAMPLE_TID
            | PERF_SAMPLE_TIME
            | PERF_SAMPLE_CPU
            | PERF_SAMPLE_PERIOD;

        let is_event_based = matches!(config.event_source, EventSource::SwContextSwitches);
        if is_event_based {
            // Event-based: sample every event (period=1), no inherit.
            // Context switches fire in kernel context, so we can't set exclude_kernel
            // on the event itself. Kernel frames in the callchain are filtered at parse
            // time via USER_ADDR_LIMIT (see parse_sample).
            attr.sample_period_or_freq = 1;
            attr.flags = PERF_ATTR_FLAG_SAMPLE_ID_ALL;
        } else {
            attr.sample_period_or_freq = config.frequency_hz;
            attr.flags =
                PERF_ATTR_FLAG_FREQ | PERF_ATTR_FLAG_SAMPLE_ID_ALL | PERF_ATTR_FLAG_INHERIT;
            if !config.include_kernel {
                attr.flags |= PERF_ATTR_FLAG_EXCLUDE_KERNEL | PERF_ATTR_FLAG_EXCLUDE_HV;
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
            if addr >= USER_ADDR_LIMIT || addr == 0 {
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

#[cfg(test)]
mod tests {
    use super::*;
    use crate::ring_buffer::RecordBody;

    /// Our standard sample_type: IP | TID | TIME | CPU | PERIOD | CALLCHAIN
    const STD_SAMPLE_TYPE: u64 = PERF_SAMPLE_IP
        | PERF_SAMPLE_TID
        | PERF_SAMPLE_TIME
        | PERF_SAMPLE_CPU
        | PERF_SAMPLE_PERIOD
        | PERF_SAMPLE_CALLCHAIN;

    /// Build a fake sample body matching STD_SAMPLE_TYPE layout.
    fn build_body(
        ip: u64,
        pid: u32,
        tid: u32,
        time: u64,
        cpu: u32,
        period: u64,
        addrs: &[u64],
    ) -> Vec<u8> {
        let mut buf = Vec::new();
        buf.extend_from_slice(&ip.to_ne_bytes()); // IP
        buf.extend_from_slice(&pid.to_ne_bytes()); // TID.pid
        buf.extend_from_slice(&tid.to_ne_bytes()); // TID.tid
        buf.extend_from_slice(&time.to_ne_bytes()); // TIME
        buf.extend_from_slice(&cpu.to_ne_bytes()); // CPU
        buf.extend_from_slice(&0u32.to_ne_bytes()); // CPU.res
        buf.extend_from_slice(&period.to_ne_bytes()); // PERIOD
        buf.extend_from_slice(&(addrs.len() as u64).to_ne_bytes()); // nr
        for &a in addrs {
            buf.extend_from_slice(&a.to_ne_bytes());
        }
        buf
    }

    #[test]
    fn parse_sample_basic() {
        let body = build_body(0x4000, 100, 200, 999, 2, 1, &[0x4000, 0x4100]);
        let s = parse_sample(STD_SAMPLE_TYPE, &RecordBody::Contiguous(&body)).unwrap();
        assert_eq!(s.ip, 0x4000);
        assert_eq!(s.pid, 100);
        assert_eq!(s.tid, 200);
        assert_eq!(s.time, 999);
        assert_eq!(s.cpu, 2);
        assert_eq!(s.period, 1);
        assert_eq!(s.callchain, vec![0x4000, 0x4100]);
    }

    #[test]
    fn parse_sample_filters_kernel_addrs() {
        let kernel_addr = USER_ADDR_LIMIT + 1;
        let body = build_body(0x4000, 1, 1, 1, 0, 1, &[0x4000, kernel_addr, 0x4100, 0]);
        let s = parse_sample(STD_SAMPLE_TYPE, &RecordBody::Contiguous(&body)).unwrap();
        // kernel addr and 0 should be filtered out
        assert_eq!(s.callchain, vec![0x4000, 0x4100]);
    }

    #[test]
    fn parse_sample_filters_context_markers() {
        let body = build_body(
            0x4000,
            1,
            1,
            1,
            0,
            1,
            &[PERF_CONTEXT_KERNEL, 0x4000, PERF_CONTEXT_USER, 0x4100],
        );
        let s = parse_sample(STD_SAMPLE_TYPE, &RecordBody::Contiguous(&body)).unwrap();
        // Context markers are >= USER_ADDR_LIMIT, so filtered
        assert_eq!(s.callchain, vec![0x4000, 0x4100]);
    }

    #[test]
    fn parse_sample_truncated_body() {
        // Body too short to even read IP
        let body = vec![0u8; 4];
        assert!(parse_sample(STD_SAMPLE_TYPE, &RecordBody::Contiguous(&body)).is_none());
    }

    #[test]
    fn parse_sample_rejects_huge_callchain() {
        let body = build_body(0x4000, 1, 1, 1, 0, 1, &[]);
        let mut buf = body[..body.len() - 8].to_vec(); // remove the nr=0
        buf.extend_from_slice(&2000u64.to_ne_bytes()); // nr=2000 > 1024
        assert!(parse_sample(STD_SAMPLE_TYPE, &RecordBody::Contiguous(&buf)).is_none());
    }

    #[test]
    fn parse_sample_empty_callchain() {
        let body = build_body(0x4000, 1, 1, 1, 0, 1, &[]);
        let s = parse_sample(STD_SAMPLE_TYPE, &RecordBody::Contiguous(&body)).unwrap();
        assert!(s.callchain.is_empty());
    }

    // --- get_online_cpus tests ---

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
