pub mod analysis;
pub(crate) mod buffer;
pub(crate) mod collector;
pub use collector::Batch;
#[cfg(feature = "cpu-profiling")]
pub mod cpu_profile;
pub mod events;
pub mod format;
pub mod recorder;
pub mod task_metadata;
pub mod writer;

pub use analysis::{
    ActivePeriod, LongPoll, SampledPoll, SchedDelay, SpawnLocationStats, TraceAnalysis,
    TraceReader, WakeDelay, WorkerStats, analyze_trace, compute_active_periods,
    compute_wake_to_poll_delays, detect_idle_workers, detect_long_polls, detect_sampled_polls,
    detect_sched_delays, detect_wake_delays, print_analysis,
};
#[cfg(feature = "cpu-profiling")]
pub use cpu_profile::CpuProfilingConfig;
#[cfg(feature = "cpu-profiling")]
pub use cpu_profile::SchedEventConfig;
pub use dial9_trace_format::InternedString;
pub use events::{CpuSampleData, CpuSampleSource, SchedStat, TelemetryEvent};
pub use format::WorkerId;
pub use recorder::{
    HasTracePath, NoTracePath, TelemetryGuard, TelemetryHandle, TracedRuntime, TracedRuntimeBuilder,
};
pub use task_metadata::{TaskId, UNKNOWN_TASK_ID};
pub use writer::{NullWriter, RotatingWriter, TraceWriter};
