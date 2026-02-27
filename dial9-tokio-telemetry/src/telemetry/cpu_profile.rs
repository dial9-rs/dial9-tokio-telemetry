//! CPU profiling integration: merges perf stack traces into the telemetry stream.
//!
//! When enabled, a process-wide `PerfSampler` captures CPU stack traces at a
//! configurable frequency. The flush thread drains samples, maps OS thread IDs
//! to tokio worker IDs, and writes `CpuSample` events into the trace.

use crate::telemetry::events::{TelemetryEvent, UNKNOWN_WORKER};
use perf_self_profile::{EventSource, PerfSampler, SamplerConfig};
use std::collections::HashMap;
use std::io;

/// Read the thread name from `/proc/self/task/<tid>/comm`.
/// Returns `None` if the file can't be read.
pub(crate) fn read_thread_name(tid: u32) -> Option<String> {
    std::fs::read_to_string(format!("/proc/self/task/{tid}/comm"))
        .ok()
        .map(|s| s.trim().to_string())
        .filter(|s| !s.is_empty())
}

/// Configuration for CPU profiling integration.
#[derive(Debug, Clone)]
pub struct CpuProfilingConfig {
    /// Sampling frequency in Hz. Default: 99 (low overhead).
    pub frequency_hz: u64,
    /// Which perf event source to use.
    pub event_source: EventSource,
    /// Whether to include kernel stack frames.
    pub include_kernel: bool,
}

impl Default for CpuProfilingConfig {
    fn default() -> Self {
        Self {
            frequency_hz: 99,
            event_source: EventSource::SwCpuClock,
            include_kernel: false,
        }
    }
}

/// Configuration for per-worker sched event capture (context switches).
///
/// Uses `perf_event_open` with `SwContextSwitches` in per-thread mode,
/// so each worker thread gets its own perf fd via `on_thread_start`.
#[derive(Debug, Clone, Default)]
pub struct SchedEventConfig {
    /// Whether to include kernel stack frames.
    pub include_kernel: bool,
}

/// Manages the perf sampler and converts samples to telemetry events.
pub(crate) struct CpuProfiler {
    sampler: PerfSampler,
    /// Offset to convert perf timestamps (CLOCK_MONOTONIC nanos, via use_clockid) to trace-relative nanos.
    /// trace_ns = perf_time - clock_offset
    clock_offset: u64,
    /// OS tid → worker_id mapping, populated from SharedState.worker_tids.
    tid_to_worker: HashMap<u32, usize>,
    /// OS tid → thread name, eagerly cached at drain time so short-lived threads
    /// are captured before they exit and `/proc/self/task/<tid>/comm` disappears.
    tid_to_name: HashMap<u32, String>,
}

impl CpuProfiler {
    /// Start the profiler. Captures the clock offset for timestamp correlation.
    ///
    /// `trace_start_mono` is `clock_gettime(CLOCK_MONOTONIC)` captured at the same
    /// moment as the telemetry `Instant::now()` start time.
    pub fn start(config: CpuProfilingConfig, trace_start_mono_ns: u64) -> io::Result<Self> {
        let sampler = PerfSampler::start(SamplerConfig {
            frequency_hz: config.frequency_hz,
            event_source: config.event_source,
            include_kernel: config.include_kernel,
        })?;
        Ok(Self {
            sampler,
            clock_offset: trace_start_mono_ns,
            tid_to_worker: HashMap::new(),
            tid_to_name: HashMap::new(),
        })
    }

    /// Update the tid→worker mapping.
    pub fn register_worker(&mut self, worker_id: usize, tid: u32) {
        self.tid_to_worker.insert(tid, worker_id);
    }

    /// Drain all pending perf samples and convert to CpuSample events.
    ///
    /// Eagerly reads `/proc/self/task/<tid>/comm` for non-worker tids so that
    /// thread names are captured before short-lived threads exit.
    pub fn drain(&mut self) -> Vec<TelemetryEvent> {
        let mut events = Vec::new();
        self.sampler.for_each_sample(|sample| {
            let timestamp_nanos = sample.time.saturating_sub(self.clock_offset);
            let worker_id = self
                .tid_to_worker
                .get(&sample.tid)
                .copied()
                .unwrap_or(UNKNOWN_WORKER);
            // Eagerly cache thread name for non-worker tids while the thread
            // is still alive and /proc/self/task/<tid>/comm is readable.
            if worker_id == UNKNOWN_WORKER
                && !self.tid_to_name.contains_key(&sample.tid)
                && let Some(name) = read_thread_name(sample.tid)
            {
                self.tid_to_name.insert(sample.tid, name);
            }
            events.push(TelemetryEvent::CpuSample {
                timestamp_nanos,
                worker_id,
                tid: sample.tid,
                source: crate::telemetry::events::CpuSampleSource::CpuProfile,
                callchain: sample.callchain.clone(),
            });
        });
        events
    }

    /// Return the cached thread name for a non-worker tid, if known.
    pub fn thread_name(&self, tid: u32) -> Option<&str> {
        self.tid_to_name.get(&tid).map(|s| s.as_str())
    }
}

/// Read `CLOCK_MONOTONIC` in nanoseconds.
pub(crate) fn clock_monotonic_ns() -> u64 {
    let mut ts = libc::timespec {
        tv_sec: 0,
        tv_nsec: 0,
    };
    unsafe {
        libc::clock_gettime(libc::CLOCK_MONOTONIC, &mut ts);
    }
    ts.tv_sec as u64 * 1_000_000_000 + ts.tv_nsec as u64
}

/// Per-thread sched event profiler. Each worker thread calls `track_current_thread`
/// from `on_thread_start`; the flush thread drains samples via `drain`.
pub(crate) struct SchedProfiler {
    sampler: PerfSampler,
    clock_offset: u64,
    tid_to_worker: HashMap<u32, usize>,
}

impl SchedProfiler {
    pub fn new(config: SchedEventConfig, trace_start_mono_ns: u64) -> io::Result<Self> {
        let sampler = PerfSampler::new_per_thread(SamplerConfig {
            frequency_hz: 1, // ignored for event-based; period=1
            event_source: EventSource::SwContextSwitches,
            include_kernel: config.include_kernel,
        })?;
        Ok(Self {
            sampler,
            clock_offset: trace_start_mono_ns,
            tid_to_worker: HashMap::new(),
        })
    }

    pub fn track_current_thread(&mut self) -> io::Result<()> {
        self.sampler.track_current_thread()
    }

    pub fn stop_tracking_current_thread(&mut self) {
        self.sampler.stop_tracking_current_thread()
    }

    pub fn register_worker(&mut self, worker_id: usize, tid: u32) {
        self.tid_to_worker.insert(tid, worker_id);
    }

    pub fn drain(&mut self) -> Vec<TelemetryEvent> {
        let mut events = Vec::new();
        self.sampler.for_each_sample(|sample| {
            let timestamp_nanos = sample.time.saturating_sub(self.clock_offset);
            let worker_id = self
                .tid_to_worker
                .get(&sample.tid)
                .copied()
                .unwrap_or(UNKNOWN_WORKER);
            events.push(TelemetryEvent::CpuSample {
                timestamp_nanos,
                worker_id,
                tid: sample.tid,
                source: crate::telemetry::events::CpuSampleSource::SchedEvent,
                callchain: sample.callchain.clone(),
            });
        });
        events
    }
}
