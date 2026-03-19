//! Per-stage pipeline metrics using the MultiFlex pattern.
//!
//! Each [`SegmentProcessor`](super::SegmentProcessor) stage gets its own
//! [`StageMetrics`] (timer + success flag). When the entry is written,
//! each stage's metrics are prefixed with the stage name, producing keys
//! like `Gzip.Time`, `Gzip.Success`, `S3Upload.Time`, etc.

use metrique::timers::Timer;
use metrique::unit::Millisecond;
use metrique::unit_of_work::metrics;
use metrique::{CloseValue, CloseValueRef, InflectableEntry, NameStyle};
use metrique_writer::{EntryWriter, Value};
use std::borrow::Cow;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum MetriqueResult {
    Success,
    Failure,
}

impl CloseValue for MetriqueResult {
    type Closed = <MetriqueResultEntry as CloseValue>::Closed;

    fn close(self) -> Self::Closed {
        self.close_ref()
    }
}

impl CloseValue for &'_ MetriqueResult {
    type Closed = <MetriqueResultEntry as CloseValue>::Closed;

    fn close(self) -> Self::Closed {
        match self {
            MetriqueResult::Success => MetriqueResultEntry {
                success: true,
                failure: false,
            }
            .close(),
            MetriqueResult::Failure => MetriqueResultEntry {
                success: false,
                failure: true,
            }
            .close(),
        }
    }
}

#[metrics(subfield)]
#[derive(Debug)]
pub struct MetriqueResultEntry {
    success: bool,
    failure: bool,
}

/// Metrics for a single pipeline stage.
#[metrics(subfield, rename_all = "PascalCase")]
#[derive(Debug)]
pub(crate) struct StageMetrics {
    #[metrics(unit = Millisecond)]
    pub(crate) time: Timer,
    #[metrics(flatten)]
    pub(crate) status: Option<MetriqueResult>,
}

impl StageMetrics {
    pub(crate) fn start() -> Self {
        Self {
            time: Timer::start_now(),
            status: None,
        }
    }

    pub(crate) fn succeed(&mut self) {
        self.status = Some(MetriqueResult::Success);
        self.time.stop();
    }

    pub(crate) fn fail(&mut self) {
        self.status = Some(MetriqueResult::Failure);
        self.time.stop();
    }
}

/// Collects per-stage metrics for the segment processing pipeline.
#[derive(Debug, Default)]
pub(crate) struct PipelineMetrics {
    stages: Vec<(&'static str, StageMetrics)>,
}

impl PipelineMetrics {
    pub(crate) fn push(&mut self, name: &'static str, metrics: StageMetrics) {
        self.stages.push((name, metrics));
    }
}

/// Closed form of [`PipelineMetrics`] with pre-closed stage entries.
#[derive(Debug)]
pub(crate) struct PipelineMetricsEntry {
    stages: Vec<(&'static str, <StageMetrics as CloseValue>::Closed)>,
}

impl CloseValue for PipelineMetrics {
    type Closed = PipelineMetricsEntry;

    fn close(self) -> Self::Closed {
        PipelineMetricsEntry {
            stages: self
                .stages
                .into_iter()
                .map(|(name, stage)| (name, stage.close_ref()))
                .collect(),
        }
    }
}

impl CloseValue for &PipelineMetrics {
    type Closed = PipelineMetricsEntry;

    fn close(self) -> Self::Closed {
        PipelineMetricsEntry {
            stages: self
                .stages
                .iter()
                .map(|(name, stage)| (*name, stage.close_ref()))
                .collect(),
        }
    }
}

/// A writer adapter that prepends a dynamic prefix to all metric names.
struct PrefixWriter<'b, W> {
    prefix: &'b str,
    writer: &'b mut W,
}

impl<'a, 'b, W: EntryWriter<'a>> EntryWriter<'a> for PrefixWriter<'b, W> {
    fn timestamp(&mut self, timestamp: std::time::SystemTime) {
        self.writer.timestamp(timestamp);
    }

    fn value(&mut self, name: impl Into<Cow<'a, str>>, value: &(impl Value + ?Sized)) {
        self.writer
            .value(format!("{}{}", self.prefix, name.into()), value);
    }

    fn config(&mut self, config: &'a dyn metrique_writer::EntryConfig) {
        self.writer.config(config);
    }
}

impl<NS: NameStyle> InflectableEntry<NS> for PipelineMetricsEntry {
    fn write<'a>(&'a self, writer: &mut impl EntryWriter<'a>) {
        for (name, closed) in &self.stages {
            let prefix = format!("{}.", name);
            let mut prefixer = PrefixWriter {
                prefix: &prefix,
                writer,
            };
            InflectableEntry::<NS>::write(closed, &mut prefixer);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use assert2::check;
    use metrique_writer::test_util::test_metric;

    #[test]
    fn empty_pipeline_emits_nothing() {
        let m = PipelineMetrics::default();
        let entry = test_metric(m);
        check!(entry.metrics.is_empty());
        check!(entry.values.is_empty());
    }

    #[test]
    fn single_stage_success() {
        let mut m = PipelineMetrics::default();
        let mut stage = StageMetrics::start();
        stage.succeed();
        m.push("Gzip", stage);

        let entry = test_metric(m);
        check!(entry.metrics["Gzip.Time"].as_u64() < 1000);
        check!(entry.metrics["Gzip.Success"] == true);
        check!(entry.metrics["Gzip.Failure"] == false);
    }

    #[test]
    fn single_stage_failure() {
        let mut m = PipelineMetrics::default();
        let mut stage = StageMetrics::start();
        stage.fail();
        m.push("S3Upload", stage);

        let entry = test_metric(m);
        check!(entry.metrics["S3Upload.Success"] == false);
        check!(entry.metrics["S3Upload.Failure"] == true);
    }

    #[test]
    fn multiple_stages_prefixed_independently() {
        let mut m = PipelineMetrics::default();

        let mut s1 = StageMetrics::start();
        s1.succeed();
        m.push("Gzip", s1);

        let mut s2 = StageMetrics::start();
        s2.fail();
        m.push("S3Upload", s2);

        let entry = test_metric(m);
        check!(entry.metrics["Gzip.Success"] == true);
        check!(entry.metrics["Gzip.Failure"] == false);
        check!(entry.metrics["S3Upload.Success"] == false);
        check!(entry.metrics["S3Upload.Failure"] == true);
        check!(entry.metrics.contains_key("Gzip.Time"));
        check!(entry.metrics.contains_key("S3Upload.Time"));
    }

    /// Not a real assertion test — prints the full SegmentProcessMetrics shape
    /// so you can see what the metric output looks like end-to-end.
    #[test]
    fn show_full_segment_metrics() {
        use crate::metrics::{Operation, SegmentProcessMetrics};
        use metrique::timers::Timer;

        let mut pipeline = PipelineMetrics::default();
        let mut gzip = StageMetrics::start();
        gzip.succeed();
        pipeline.push("Gzip", gzip);

        let mut upload = StageMetrics::start();
        upload.succeed();
        pipeline.push("S3Upload", upload);

        let m = SegmentProcessMetrics {
            operation: Operation::ProcessSegment,
            total_time: Timer::start_now(),
            status: Some(super::MetriqueResult::Success),
            segment_index: 7,
            uncompressed_size: 65536,
            compressed_size: Some(12345),
            invalid_file_header: false,
            pipeline,
        };

        let entry = test_metric(m);

        // Verify the pipeline stage keys are present alongside the top-level keys
        check!(entry.metrics.contains_key("TotalTime"));
        check!(entry.metrics.contains_key("Success"));
        check!(entry.metrics.contains_key("Failure"));
        check!(entry.metrics.contains_key("SegmentIndex"));
        check!(entry.metrics.contains_key("UncompressedSize"));
        check!(entry.metrics.contains_key("CompressedSize"));
        check!(entry.metrics.contains_key("InvalidFileHeader"));
        check!(entry.metrics["InvalidFileHeader"] == false);
        check!(entry.metrics.contains_key("Gzip.Time"));
        check!(entry.metrics.contains_key("Gzip.Success"));
        check!(entry.metrics.contains_key("Gzip.Failure"));
        check!(entry.metrics.contains_key("S3Upload.Time"));
        check!(entry.metrics.contains_key("S3Upload.Success"));
        check!(entry.metrics.contains_key("S3Upload.Failure"));
    }
}
