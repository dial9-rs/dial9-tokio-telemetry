//! CPU profiling integration: merges perf stack traces into the telemetry stream.
//!
//! When enabled, a process-wide `PerfSampler` captures CPU stack traces at a
//! configurable frequency. The flush thread drains samples, maps OS thread IDs
//! to tokio worker IDs, and writes `CpuSample` events into the trace.

use crate::telemetry::events::TelemetryEvent;
use perf_self_profile::{EventSource, PerfSampler, SamplerConfig};
use std::collections::HashMap;
use std::io;

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

/// Manages the perf sampler and converts samples to telemetry events.
pub(crate) struct CpuProfiler {
    sampler: PerfSampler,
    /// Offset to convert perf timestamps (CLOCK_MONOTONIC nanos) to trace-relative nanos.
    /// trace_ns = perf_time - clock_offset
    clock_offset: u64,
    /// OS tid → worker_id mapping, populated from SharedState.worker_tids.
    tid_to_worker: HashMap<u32, usize>,
}

/// Sentinel for samples from non-worker threads.
const UNKNOWN_WORKER: usize = 255;

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
        })
    }

    /// Update the tid→worker mapping.
    pub fn register_worker(&mut self, worker_id: usize, tid: u32) {
        self.tid_to_worker.insert(tid, worker_id);
    }

    /// Check if the sampler has pending data.
    pub fn has_pending(&self) -> bool {
        self.sampler.has_pending()
    }

    /// Drain all pending perf samples and convert to CpuSample events.
    pub fn drain(&mut self) -> Vec<TelemetryEvent> {
        let has_data = self.sampler.has_pending();
        let mut events = Vec::new();
        eprintln!(
            "drainling sampler... {has_data} {}",
            self.tid_to_worker.len()
        );
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
                callchain: sample.callchain.clone(),
            });
        });
        if has_data || !events.is_empty() {
            eprintln!(
                "[cpu-profiler] has_pending={has_data} drained={} workers_mapped={}",
                events.len(),
                self.tid_to_worker.len()
            );
        }
        events
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
