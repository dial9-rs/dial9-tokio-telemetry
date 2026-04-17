//! Shared types for the sampler: event source, configuration, and sample data.

/// Which event source to sample on.
// TODO: these variants are currently Linux-specific (perf_event_open constants),
// consider cfg-gating individual variants when adding other platform backends.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[non_exhaustive]
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
    /// A kernel tracepoint, identified by its tracepoint ID.
    ///
    /// The ID comes from `/sys/kernel/debug/tracing/events/<subsystem>/<event>/id`.
    /// Samples include raw tracepoint data accessible via [`Sample::raw`].
    Tracepoint(u32),
}

/// The sampling mode to use.
#[derive(Debug, Clone, Copy)]
#[non_exhaustive]
pub enum SamplingMode {
    /// Sample at this frequency in Hz (e.g., 999 or 4000).
    FrequencyHz(u64),
    /// Record one sample per this many events. `1` = every event.
    Period(u64),
}

/// Configuration for the sampler.
#[derive(Debug, Clone)]
pub struct SamplerConfig {
    pub(crate) event_source: EventSource,
    pub(crate) sampling: SamplingMode,
    pub(crate) include_kernel: bool,
}

impl Default for SamplerConfig {
    fn default() -> Self {
        SamplerConfig {
            sampling: SamplingMode::FrequencyHz(999),
            event_source: EventSource::SwCpuClock,
            include_kernel: false,
        }
    }
}

impl SamplerConfig {
    /// Set the event source to sample on.
    pub fn event_source(mut self, source: EventSource) -> Self {
        self.event_source = source;
        self
    }

    /// Set the sampling mode (frequency or fixed period).
    pub fn sampling(mut self, mode: SamplingMode) -> Self {
        self.sampling = mode;
        self
    }

    /// Whether to include kernel stack frames.
    /// Requires `perf_event_paranoid` <= 1 (or CAP_PERFMON).
    pub fn include_kernel(mut self, yes: bool) -> Self {
        self.include_kernel = yes;
        self
    }
}

/// A single sample captured from perf events.
#[derive(Debug, Clone)]
#[non_exhaustive]
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
    /// Raw tracepoint data, present only for [`EventSource::Tracepoint`] events.
    /// Parse with [`TracepointDef::extract_fields`](crate::tracepoint::TracepointDef::extract_fields).
    pub raw: Option<Vec<u8>>,
}
