//! Operational metrics published via metrique.

use std::time::Duration;

use crate::background_task::pipeline_metrics::{MetriqueResult, PipelineMetrics};
use metrique::timers::Timer;
use metrique::unit::{Byte, Microsecond, Millisecond};
use metrique::unit_of_work::metrics;

/// Distinguishes the type of operation a metric entry describes.
#[derive(Clone, Copy)]
#[metrics(value(string))]
pub(crate) enum Operation {
    Flush,
    ProcessSegment,
}

// https://github.com/awslabs/metrique/issues/250
impl std::fmt::Debug for Operation {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Flush => write!(f, "Flush"),
            Self::ProcessSegment => write!(f, "ProcessSegment"),
        }
    }
}

impl std::fmt::Debug for OperationValue {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(<&str>::from(self))
    }
}

/// Metrics emitted by the flush thread each cycle.
#[metrics(rename_all = "PascalCase")]
#[derive(Debug)]
pub(crate) struct FlushMetrics {
    pub operation: Operation,
    /// Number of events written in this flush cycle.
    pub event_count: u64,
    /// Wall-clock time spent draining and writing.
    #[metrics(unit = Microsecond)]
    pub flush_duration: Timer,
    /// Oldest batches evicted since last flush.
    pub dropped_batches: u64,

    /// Duration spent flushing CPU metircs
    #[metrics(unit = Microsecond)]
    pub cpu_flush_duration: Duration,

    /// The last flush during shutdown
    pub last_flush: bool,
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
