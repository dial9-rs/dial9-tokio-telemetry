//! perf_event_open-based CPU sampler backend.
//!
//! Uses the kernel's `perf_event_open` syscall with mmap ring buffers for
//! zero-copy sample delivery. Each event fd has its own ring buffer; samples
//! are parsed via `perf-event-data`.

use std::io;
use std::ptr;

use perf_event_data::Record;
use perf_event_data::endian::Little;
use perf_event_data::parse::{ParseConfig, Parser};
use perf_event_open_sys::bindings::{
    PERF_CONTEXT_MAX as PERF_CONTEXT_START_MARKER, PERF_COUNT_HW_CPU_CYCLES,
    PERF_COUNT_SW_CONTEXT_SWITCHES, PERF_COUNT_SW_CPU_CLOCK, PERF_COUNT_SW_TASK_CLOCK,
    PERF_FLAG_FD_CLOEXEC, PERF_SAMPLE_CALLCHAIN, PERF_SAMPLE_CPU, PERF_SAMPLE_IP,
    PERF_SAMPLE_PERIOD, PERF_SAMPLE_RAW, PERF_SAMPLE_TID, PERF_SAMPLE_TIME, PERF_TYPE_HARDWARE,
    PERF_TYPE_SOFTWARE, PERF_TYPE_TRACEPOINT, perf_event_attr,
};

use super::USER_ADDR_LIMIT;
use super::gettid;
use super::ring_buffer::{RingBuffer, page_size};
use crate::sampler::{EventSource, Sample, SamplerConfig};

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

const PAGE_COUNT: usize = 512; // power of 2; 2 MB at 4 KB pages

/// Open a perf event fd, mmap the ring buffer, and enable it.
fn open_perf_event(attr: &mut perf_event_attr, pid: i32, cpu: i32) -> io::Result<PerfEvent> {
    let page_size = page_size();
    let data_size = PAGE_COUNT * page_size;
    let mmap_size = (PAGE_COUNT + 1) * page_size;

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
        return Err(io::Error::last_os_error());
    }

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
        return Err(err);
    }

    let ring = unsafe { RingBuffer::new(base as *mut u8, data_size as u64, mmap_size) };

    if unsafe { perf_event_open_sys::ioctls::ENABLE(fd, 0) } < 0 {
        let err = io::Error::last_os_error();
        unsafe { libc::close(fd) };
        return Err(err);
    }

    Ok(PerfEvent { fd, ring, tid: pid })
}

pub(super) struct PerfSamplerImpl {
    events: Vec<PerfEvent>,
    parse_config: ParseConfig<Little>,
    attr: perf_event_attr,
    include_kernel: bool,
}

impl PerfSamplerImpl {
    pub(super) fn start_for_pid(pid: i32, config: &SamplerConfig) -> io::Result<Self> {
        let mut attr = Self::build_attr(config)?;

        let is_event_based = matches!(
            config.event_source,
            EventSource::SwContextSwitches | EventSource::Tracepoint(_)
        );
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
                events.push(open_perf_event(&mut attr, pid, cpu)?);
            }
        }

        Ok(PerfSamplerImpl {
            events,
            parse_config: ParseConfig::from(attr),
            attr,
            include_kernel: config.include_kernel,
        })
    }

    pub(super) fn new_per_thread(config: &SamplerConfig) -> io::Result<Self> {
        let mut attr = Self::build_attr(config)?;

        // Probe perf_event_open to detect if it's blocked (e.g., seccomp on
        // ECS/Fargate). Without this, creation succeeds but every subsequent
        // track_current_thread() call fails silently.
        let probe_fd = unsafe {
            perf_event_open_sys::perf_event_open(
                &mut attr,
                0,  // current thread
                -1, // any CPU
                -1, // no group
                PERF_FLAG_FD_CLOEXEC as libc::c_ulong,
            )
        };
        if probe_fd < 0 {
            return Err(io::Error::last_os_error());
        }
        unsafe { libc::close(probe_fd) };

        Ok(PerfSamplerImpl {
            events: Vec::new(),
            parse_config: ParseConfig::from(attr),
            attr,
            include_kernel: config.include_kernel,
        })
    }

    fn build_attr(config: &SamplerConfig) -> io::Result<perf_event_attr> {
        use crate::SamplingMode;

        match config.sampling {
            SamplingMode::FrequencyHz(0) => {
                return Err(io::Error::new(
                    io::ErrorKind::InvalidInput,
                    "FrequencyHz(0) is invalid",
                ));
            }
            SamplingMode::FrequencyHz(hz) => {
                // Check max sample rate
                if let Ok(contents) =
                    std::fs::read_to_string("/proc/sys/kernel/perf_event_max_sample_rate")
                    && let Ok(max_rate) = contents.trim().parse::<u64>()
                    && hz > max_rate
                {
                    return Err(io::Error::new(
                        io::ErrorKind::InvalidInput,
                        format!(
                            "requested frequency {} exceeds kernel max {} \
                             (see /proc/sys/kernel/perf_event_max_sample_rate)",
                            hz, max_rate
                        ),
                    ));
                }
            }
            SamplingMode::Period(0) => {
                return Err(io::Error::new(
                    io::ErrorKind::InvalidInput,
                    "Period must be >= 1",
                ));
            }
            SamplingMode::Period(_) => {}
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
            EventSource::Tracepoint(id) => {
                attr.type_ = PERF_TYPE_TRACEPOINT;
                attr.config = id as u64;
            }
        }

        let is_event_based = matches!(
            config.event_source,
            EventSource::SwContextSwitches | EventSource::Tracepoint(_)
        );

        attr.sample_type = PERF_SAMPLE_IP as u64
            | PERF_SAMPLE_CALLCHAIN as u64
            | PERF_SAMPLE_TID as u64
            | PERF_SAMPLE_TIME as u64
            | PERF_SAMPLE_CPU as u64
            // Omit PERF_SAMPLE_PERIOD in Period mode. When set, perf_swevent_event()
            // emits a sample on every SW event, skipping the countdown
            // that implements 1-in-N sampling.
            | if matches!(config.sampling, SamplingMode::FrequencyHz(_)) {
                PERF_SAMPLE_PERIOD as u64
            } else {
                0
            }
            // PERF_SAMPLE_RAW includes the tracepoint's raw event data (field
            // values) in each sample. Only tracepoints produce meaningful raw
            // data; CPU and context-switch sources have nothing to attach.
            | if matches!(config.event_source, EventSource::Tracepoint(_)) {
                PERF_SAMPLE_RAW as u64
            } else {
                0
            };

        // Stamp samples with CLOCK_MONOTONIC so they share a clock with ctimer
        // samples and `telemetry::events::clock_monotonic_ns()`.
        attr.set_use_clockid(1);
        attr.clockid = libc::CLOCK_MONOTONIC;

        attr.set_disabled(1);
        match config.sampling {
            SamplingMode::Period(period) => {
                // exclude_kernel must remain 0 since context switches fire in
                // kernel context; kernel callchain frames are filtered at
                // parse time via USER_ADDR_LIMIT.
                attr.sample_period = period;
                attr.wakeup_events = 1;
                attr.set_sample_id_all(1);
            }
            SamplingMode::FrequencyHz(hz) => {
                attr.sample_freq = hz;
                attr.set_freq(1);
                attr.set_sample_id_all(1);
                if !is_event_based {
                    attr.set_inherit(1);
                    if !config.include_kernel {
                        attr.set_exclude_kernel(1);
                        attr.set_exclude_hv(1);
                    }
                }
            }
        }

        Ok(attr)
    }
}

impl super::sampler::SamplerBackend for PerfSamplerImpl {
    // Per-thread mode: call from the thread you want to monitor.
    // This opens an event fd scoped to the calling tid with cpu=-1.
    fn track_current_thread(&mut self) -> io::Result<()> {
        let mut ev = open_perf_event(&mut self.attr, 0, -1)?;
        // open_perf_event stores tid=0 (the "current thread" sentinel passed to
        // perf_event_open). Resolve to the real tid so stop_tracking_current_thread
        // can find this event later.
        ev.tid = gettid() as i32;
        self.events.push(ev);
        Ok(())
    }

    /// Must be called from the same thread that called track_current_thread.
    /// Dropping the event closes fd and unmaps ring, unread samples are lost.
    fn stop_tracking_current_thread(&mut self) {
        let tid = gettid() as i32;
        if let Some(idx) = self.events.iter().position(|ev| ev.tid == tid) {
            self.events.swap_remove(idx);
        }
    }

    fn has_pending(&self) -> bool {
        self.events.iter().any(|ev| ev.ring.has_data())
    }

    /// Non-blocking drain of all currently available records.
    fn for_each_sample(&mut self, f: &mut dyn FnMut(&Sample)) {
        let parse_config = self.parse_config.clone();

        for ev in &mut self.events {
            ev.ring.for_each_record(|record| {
                let mut buf = Vec::with_capacity(record.header.size as usize);
                let header_bytes: [u8; 8] = unsafe { std::mem::transmute(record.header) };
                buf.extend_from_slice(&header_bytes);
                match &record.body {
                    super::ring_buffer::RecordBody::Contiguous(data) => buf.extend_from_slice(data),
                    super::ring_buffer::RecordBody::Split(a, b) => {
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
                        let include_kernel = self.include_kernel;
                        let callchain =
                            filter_callchain(s.callchain().unwrap_or(&[]), include_kernel);
                        Sample {
                            ip: s.ip().unwrap_or(0),
                            pid: s.pid().unwrap_or(0),
                            tid: s.tid().unwrap_or(0),
                            time: s.time().unwrap_or(0),
                            cpu: s.cpu().unwrap_or(0),
                            period: s.period().unwrap_or(0),
                            callchain,
                            raw: s.raw().map(|r| r.to_vec()),
                        }
                    }
                    Record::Lost(lost) => {
                        tracing::debug!("[perf] lost {} events (ring buffer overflow)", lost.lost);
                        return;
                    }
                    _ => return,
                };

                f(&sample);
            });
        }
    }

    fn drain_samples(&mut self) -> Vec<Sample> {
        let mut samples = Vec::new();
        self.for_each_sample(&mut |s| samples.push(s.clone()));
        samples
    }

    /// Pause collection. Ring buffers remain mapped and pending samples can
    /// still be drained. Reversible via `enable`.
    fn disable(&self) {
        for ev in &self.events {
            unsafe { perf_event_open_sys::ioctls::DISABLE(ev.fd, 0) };
        }
    }

    /// Resume collection after disable().
    fn enable(&self) {
        for ev in &self.events {
            unsafe { perf_event_open_sys::ioctls::ENABLE(ev.fd, 0) };
        }
    }
}

/// Filter a raw perf callchain, removing zero addresses, perf context markers
/// (PERF_CONTEXT_KERNEL, PERF_CONTEXT_USER, etc.), and optionally kernel addresses.
fn filter_callchain(raw: &[u64], include_kernel: bool) -> Vec<u64> {
    raw.iter()
        .copied()
        .filter(|&a| {
            a != 0 && a < PERF_CONTEXT_START_MARKER && (include_kernel || a < USER_ADDR_LIMIT)
        })
        .collect()
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
        let cpus = get_online_cpus().expect("should read online cpus");
        assert!(
            !cpus.is_empty(),
            "system should have at least one online CPU"
        );
        assert!(cpus.iter().all(|&c| c >= 0));
        for w in cpus.windows(2) {
            assert!(w[0] < w[1], "expected sorted unique CPUs, got {:?}", cpus);
        }
    }

    #[test]
    fn filter_callchain_removes_zeros_and_context_markers() {
        let raw = [0, 0x1000, PERF_CONTEXT_START_MARKER, 0x2000, 0];
        let result = filter_callchain(&raw, false);
        assert_eq!(result, vec![0x1000, 0x2000]);
    }

    #[test]
    fn filter_callchain_excludes_kernel_addrs_when_not_included() {
        let kernel_addr = USER_ADDR_LIMIT + 0x1000;
        let user_addr = 0x5555_0000_1000u64;
        let raw = [user_addr, kernel_addr];
        assert_eq!(filter_callchain(&raw, false), vec![user_addr]);
    }

    #[test]
    fn filter_callchain_includes_kernel_addrs_when_included() {
        let kernel_addr = USER_ADDR_LIMIT + 0x1000;
        let user_addr = 0x5555_0000_1000u64;
        let raw = [user_addr, kernel_addr];
        assert_eq!(filter_callchain(&raw, true), vec![user_addr, kernel_addr]);
    }

    #[test]
    fn filter_callchain_rejects_all_context_markers() {
        let markers = [
            PERF_CONTEXT_START_MARKER,
            PERF_CONTEXT_START_MARKER + 1,
            u64::MAX,
            u64::MAX - 128,
        ];
        assert!(filter_callchain(&markers, true).is_empty());
    }
}
