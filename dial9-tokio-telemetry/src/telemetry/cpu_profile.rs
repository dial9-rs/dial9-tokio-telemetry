//! CPU profiling integration: merges perf stack traces into the telemetry stream.
//!
//! When enabled, a process-wide `PerfSampler` captures CPU stack traces at a
//! configurable frequency. The flush thread drains raw samples; the caller
//! (EventWriter) maps OS thread IDs to worker IDs via SharedState.thread_roles.

use crate::telemetry::events::{CpuSampleSource, ThreadName};
use dial9_perf_self_profile::{EventSource, PerfSampler, SamplerConfig};
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

/// A raw CPU sample before worker-id resolution.
pub(crate) struct RawCpuSample {
    pub tid: u32,
    pub timestamp_nanos: u64,
    pub callchain: Vec<u64>,
    pub source: CpuSampleSource,
}

/// Manages the process-wide perf sampler. Yields raw samples without worker IDs.
pub(crate) struct CpuProfiler {
    sampler: PerfSampler,
    pid: u32,
    /// OS tid → thread name, eagerly cached at drain time so short-lived threads
    /// are captured before they exit and `/proc/self/task/<tid>/comm` disappears.
    tid_to_name: HashMap<u32, ThreadName>,
}

impl CpuProfiler {
    pub fn start(config: CpuProfilingConfig) -> io::Result<Self> {
        let sampler = PerfSampler::start(SamplerConfig {
            frequency_hz: config.frequency_hz,
            event_source: config.event_source,
            include_kernel: config.include_kernel,
        })?;
        Ok(Self {
            sampler,
            pid: std::process::id(),
            tid_to_name: HashMap::new(),
        })
    }

    /// Drain all pending perf samples as raw (tid, callchain) tuples.
    ///
    /// Filters out child-process samples (perf `inherit` leaks them).
    /// Eagerly caches thread names for non-worker tids.
    pub fn drain(&mut self, mut f: impl FnMut(RawCpuSample, Option<&ThreadName>)) {
        let pid = self.pid;
        self.sampler.for_each_sample(|sample| {
            if sample.pid != pid {
                return;
            }
            if !self.tid_to_name.contains_key(&sample.tid)
                && let Some(name) = read_thread_name(sample.tid)
            {
                self.tid_to_name.insert(sample.tid, ThreadName::new(name));
            }
            let thread_name = self.tid_to_name.get(&sample.tid);
            f(
                RawCpuSample {
                    tid: sample.tid,
                    timestamp_nanos: sample.time,
                    callchain: sample.callchain.clone(),
                    source: CpuSampleSource::CpuProfile,
                },
                thread_name,
            );
        });
    }
}

/// Per-thread sched event profiler. Yields raw samples without worker IDs.
pub(crate) struct SchedProfiler {
    sampler: PerfSampler,
}

impl SchedProfiler {
    pub fn new(config: SchedEventConfig) -> io::Result<Self> {
        let sampler = PerfSampler::new_per_thread(SamplerConfig {
            frequency_hz: 1,
            event_source: EventSource::SwContextSwitches,
            include_kernel: config.include_kernel,
        })?;
        Ok(Self { sampler })
    }

    pub fn track_current_thread(&mut self) -> io::Result<()> {
        self.sampler.track_current_thread()
    }

    pub fn stop_tracking_current_thread(&mut self) {
        self.sampler.stop_tracking_current_thread()
    }

    pub fn drain(&mut self, mut f: impl FnMut(RawCpuSample)) {
        self.sampler.for_each_sample(|sample| {
            f(RawCpuSample {
                tid: sample.tid,
                timestamp_nanos: sample.time,
                callchain: sample.callchain.clone(),
                source: CpuSampleSource::SchedEvent,
            });
        });
    }
}
