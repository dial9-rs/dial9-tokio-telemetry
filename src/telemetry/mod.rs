pub mod analysis;
pub mod buffer;
pub mod collector;
pub mod events;
pub mod format;
pub mod recorder;
pub mod writer;

pub use analysis::{
    ActivePeriod, TraceAnalysis, TraceReader, WorkerStats, analyze_trace, compute_active_periods,
    detect_idle_workers, print_analysis,
};
pub use events::{EventType, MetricsSnapshot, SchedStat, TelemetryEvent};
pub use recorder::{TelemetryGuard, TelemetryHandle, TelemetryRecorder, TracedRuntime};
pub use writer::{NullWriter, RotatingWriter, SimpleBinaryWriter, TraceWriter};
