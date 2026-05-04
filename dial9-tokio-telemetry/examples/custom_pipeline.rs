//! Custom segment-processor pipeline.
//!
//! The dial9 worker processes each sealed trace segment by passing it through
//! a chain of [`SegmentProcessor`]s. The library ships built-ins
//! (`gzip`, `write_back`, `s3`, `symbolize`); this example shows how to plug
//! your own processors into the chain via `with_custom_pipeline`.
//!
//! Three processor patterns are demonstrated:
//!
//! 1. [`LoggingProcessor`] - stateless pass-through. The simplest possible
//!    processor: it inspects [`SegmentData`] and returns it unchanged.
//! 2. [`MetadataTagger`] - stateless with construction-time config. Adds
//!    static key/value tags into the segment's metadata map. Downstream
//!    processors (and built-ins like `s3` for object metadata) can read
//!    those keys.
//! 3. [`SizeReporter`] - stateful. Holds counters across calls behind
//!    `&mut self` and prints a running summary every N segments.
//!
//! The pipeline wires the customs first, then `gzip` + `write_back` so the
//! resulting `*.bin.gz` files are still readable by the trace tooling.
//!
//! Usage:
//!   cargo run --example custom_pipeline
//!
//! Inspect the trace afterwards (gunzip first, the analysis tools read raw bytes):
//!   gunzip /tmp/dial9-custom-pipeline/trace.0.bin.gz
//!   cargo run --features analysis --example analyze_trace -- /tmp/dial9-custom-pipeline/trace.0.bin

use std::collections::HashMap;
use std::future::Future;
use std::pin::Pin;
use std::time::Duration;

use dial9_tokio_telemetry::Dial9Config;
use dial9_tokio_telemetry::background_task::{ProcessError, SegmentData, SegmentProcessor};
use dial9_tokio_telemetry::telemetry::TelemetryHandle;

const TRACE_DIR: &str = "/tmp/dial9-custom-pipeline";

// ---------------------------------------------------------------------------
// 1. Stateless pass-through: log and forward.
// ---------------------------------------------------------------------------

/// Logs each segment as it arrives and forwards it unchanged.
#[derive(Debug, Default)]
struct LoggingProcessor;

impl SegmentProcessor for LoggingProcessor {
    fn name(&self) -> &'static str {
        "Logging"
    }

    fn process(
        &mut self,
        data: SegmentData,
    ) -> Pin<Box<dyn Future<Output = Result<SegmentData, ProcessError>> + Send + '_>> {
        Box::pin(async move {
            println!(
                "[Logging]   segment {:>3}  {:>8} bytes  metadata={:?}",
                data.segment().index(),
                data.bytes().len(),
                data.metadata(),
            );
            Ok(data)
        })
    }
}

// ---------------------------------------------------------------------------
// 2. Stateless with config: tag the segment's metadata map.
// ---------------------------------------------------------------------------

/// Inserts a fixed set of key/value pairs into the segment's metadata map.
///
/// Metadata flows alongside the bytes through the rest of the pipeline.
/// Downstream processors can read it with [`SegmentData::metadata`]; the
/// built-in `s3` uploader, for example, forwards specific keys as S3
/// object metadata. Note that some metadata keys are reserved by built-in
/// processors (e.g. `content_encoding`, `write_back_extension`), avoid
/// reusing them here.
struct MetadataTagger {
    tags: HashMap<String, String>,
}

impl MetadataTagger {
    fn new<I, K, V>(tags: I) -> Self
    where
        I: IntoIterator<Item = (K, V)>,
        K: Into<String>,
        V: Into<String>,
    {
        Self {
            tags: tags
                .into_iter()
                .map(|(k, v)| (k.into(), v.into()))
                .collect(),
        }
    }
}

impl SegmentProcessor for MetadataTagger {
    fn name(&self) -> &'static str {
        "MetadataTagger"
    }

    fn process(
        &mut self,
        mut data: SegmentData,
    ) -> Pin<Box<dyn Future<Output = Result<SegmentData, ProcessError>> + Send + '_>> {
        let tags = self.tags.clone();
        Box::pin(async move {
            data.metadata_mut().extend(tags);
            Ok(data)
        })
    }
}

// ---------------------------------------------------------------------------
// 3. Stateful: track running totals across segments.
// ---------------------------------------------------------------------------

/// Accumulates per-pipeline-instance counters. Demonstrates that processors
/// can hold state across calls - `process` takes `&mut self`.
///
/// # Panic safety
/// The worker catches panics in `process` and reuses the same instance for
/// the next segment. State updates in this processor are eager and
/// monotonically increasing, so there's no half-updated invariant to worry
/// about. If you accumulate state that must roll back on failure, do the
/// update at the end of `process` once everything else has succeeded.
#[derive(Debug, Default)]
struct SizeReporter {
    segments_seen: u64,
    bytes_seen: u64,
    report_every: u64,
}

impl SizeReporter {
    fn every(n: u64) -> Self {
        Self {
            report_every: n.max(1),
            ..Self::default()
        }
    }
}

impl SegmentProcessor for SizeReporter {
    fn name(&self) -> &'static str {
        "SizeReporter"
    }

    fn process(
        &mut self,
        data: SegmentData,
    ) -> Pin<Box<dyn Future<Output = Result<SegmentData, ProcessError>> + Send + '_>> {
        self.segments_seen += 1;
        self.bytes_seen += data.bytes().len() as u64;
        if self.segments_seen.is_multiple_of(self.report_every) {
            println!(
                "[Stats]     {} segments processed, {} bytes total",
                self.segments_seen, self.bytes_seen,
            );
        }
        Box::pin(async move { Ok(data) })
    }
}

// ---------------------------------------------------------------------------
// Workload - generate enough activity for a few segments to seal.
// ---------------------------------------------------------------------------

async fn worker_task(id: usize) {
    for _ in 0..50 {
        tokio::time::sleep(Duration::from_millis(20)).await;
        let mut acc: u64 = 0;
        for i in 0..50_000 {
            acc = acc.wrapping_add((i ^ id as u64).wrapping_mul(31));
        }
        std::hint::black_box(acc);
        tokio::task::yield_now().await;
    }
}

#[dial9_tokio_telemetry::main(config = || {
    let _ = std::fs::create_dir_all(TRACE_DIR);
    let base_path = format!("{TRACE_DIR}/trace.bin");

    Dial9Config::builder()
        .base_path(base_path)
        // Small per-file budget + short rotation period so we get several
        // sealed segments in a few seconds of work - otherwise the whole
        // run might fit in a single segment and the stateful processor
        // would never have anything to count.
        .max_file_size(512 * 1024)
        .max_total_size(16 * 1024 * 1024)
        .rotation_period(Duration::from_secs(2))
        .with_tokio(|t| { t.worker_threads(4); })
        .with_runtime(|r| r
            .with_task_tracking(true)
            .with_custom_pipeline(|p| p
                .pipe(MetadataTagger::new([
                    ("service", "custom-pipeline-demo"),
                    ("environment", "local"),
                ]))
                .pipe(LoggingProcessor)
                .pipe(SizeReporter::every(1))
                .gzip()
                .write_back()))
        .build_or_disabled()
})]
async fn main() {
    println!("Running workload, traces under {TRACE_DIR}/");

    let handle = TelemetryHandle::current();
    let tasks: Vec<_> = (0..32).map(|i| handle.spawn(worker_task(i))).collect();
    for task in tasks {
        let _ = task.await;
    }

    // Give the worker a beat to seal + process the final segment before
    // the runtime shuts down. Not required in production - `TelemetryGuard`
    // honors the configured drain timeout on drop - but it makes the demo
    // output more satisfying.
    tokio::time::sleep(Duration::from_secs(3)).await;
    println!("Done. Sealed segments are gzipped under {TRACE_DIR}/*.bin.gz");
}
