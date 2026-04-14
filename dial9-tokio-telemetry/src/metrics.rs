//! Operational metrics published via metrique.

use crate::background_task::pipeline_metrics::{MetriqueResult, PipelineMetrics};
use crate::telemetry::recorder::FlushStats;
use metrique::timers::Timer;
use metrique::unit::{Byte, Microsecond, Millisecond};
use metrique::unit_of_work::metrics;

/// Distinguishes the type of operation a metric entry describes.
#[derive(Clone, Copy, Debug)]
#[metrics(value(string))]
pub(crate) enum Operation {
    Flush,
    ProcessSegment,
    TlDrain,
}

/// Metrics emitted by the flush thread each cycle.
#[metrics(rename_all = "PascalCase")]
#[derive(Debug)]
pub(crate) struct FlushMetrics {
    pub operation: Operation,
    #[metrics(flatten)]
    pub stats: FlushStats,
    /// Wall-clock time spent draining and writing.
    #[metrics(unit = Microsecond)]
    pub flush_duration: Timer,

    /// The last flush during shutdown
    pub last_flush: bool,

    /// True when writing segment metadata failed during the final flush.
    pub write_metadata_failed: bool,

    /// True when finalizing (sealing) the segment failed during the final flush.
    pub finalize_failed: bool,
}

/// Per-cycle counters produced by the intrusive thread-local buffer
/// drain. Also used as a `#[metrics(subfield)]` so callers can flatten
/// these fields into their top-level metrics without duplication.
#[metrics(subfield)]
#[derive(Debug, Default)]
pub(crate) struct TlDrainStats {
    /// Buffers that we locked cross-thread and had pending events.
    pub buffers_flushed: u64,
    /// Buffers that we locked cross-thread (superset of `buffers_flushed`;
    /// the difference is buffers that were already empty when locked).
    pub buffers_locked: u64,
    /// Handles skipped because the owning thread self-flushed during the
    /// epoch grace period. High ratio means busy workers are self-flushing
    /// efficiently and the intrusive path is staying out of their way.
    pub buffers_skipped_busy: u64,
    /// Total events drained from idle/silent buffers this cycle.
    pub events_flushed: u64,
    /// Dead `Weak` handles pruned this cycle (threads that have exited).
    pub dead_pruned: u64,
}

/// Metrics emitted every time the flush thread runs the intrusive
/// thread-local buffer drain (~every 30s, plus on shutdown).
///
/// `events_flushed > 0` means idle/silent threads were holding events
/// that would otherwise have crossed a trace file rotation.
/// `buffers_locked` vs `buffers_flushed` shows how many locks were
/// taken for buffers that turned out to be empty (e.g., a thread that
/// self-flushed after the epoch bump but before we upgraded the
/// `Weak`).
#[metrics(rename_all = "PascalCase")]
#[derive(Debug)]
pub(crate) struct TlDrainMetrics {
    pub operation: Operation,
    /// Wall-clock time spent in `drain_all_tl_buffers`.
    #[metrics(unit = Microsecond)]
    pub duration: Timer,
    #[metrics(flatten)]
    pub stats: TlDrainStats,
    /// True when this drain ran as part of shutdown finalization.
    pub last_drain: bool,
}

/// Metrics emitted per sealed segment processed by the background worker.
#[metrics(rename_all = "PascalCase")]
#[derive(Debug)]
pub(crate) struct SegmentProcessMetrics {
    pub operation: Operation,
    #[metrics(unit = Millisecond)]
    pub total_time: Timer,
    #[metrics(flatten)]
    pub status: Option<MetriqueResult>,
    pub segment_index: u32,
    #[metrics(unit = Byte)]
    pub uncompressed_size: u64,
    #[metrics(unit = Byte)]
    pub compressed_size: Option<u64>,
    /// True when the segment file lacks a valid SegmentMetadata header.
    pub invalid_file_header: bool,
    /// Per-processor metrics, keyed by processor name.
    #[metrics(flatten)]
    pub pipeline: PipelineMetrics,
}
