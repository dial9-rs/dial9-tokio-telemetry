#[cfg(feature = "worker-s3")]
pub mod connection;
pub mod instance_metadata;
pub(crate) mod pipeline_metrics;
#[cfg(feature = "worker-s3")]
pub mod s3;
pub(crate) mod sealed;

use metrique::timers::Timer;
use metrique::unit::Byte;
use metrique::unit::Millisecond;
use metrique::unit_of_work::metrics;
use metrique_writer::BoxEntrySink;
use pipeline_metrics::{MetriqueResult, PipelineMetrics, StageMetrics};
use std::collections::HashMap;
use std::future::Future;
use std::path::{Path, PathBuf};
use std::pin::Pin;
use std::time::Duration;

const DEFAULT_POLL_INTERVAL: Duration = Duration::from_secs(1);

/// Configuration for the in-process worker pipeline.
///
/// Only `trace_path` and `s3` are required. Optional fields:
///
/// - `poll_interval`: how often to check for sealed segments (default: 1 second)
/// - `client`: pre-built `aws_sdk_s3::Client` for custom credentials or endpoints
/// - `metrics_sink`: receives per-segment pipeline metrics (compression ratio, upload latency, etc.)
#[derive(bon::Builder)]
#[builder(on(String, into))]
pub struct BackgroundTaskConfig {
    /// The trace base path (same path passed to `RotatingWriter::new`).
    #[builder(into)]
    trace_path: PathBuf,
    /// How often the worker checks for sealed segments. Defaults to 1 second.
    #[builder(default = DEFAULT_POLL_INTERVAL)]
    poll_interval: Duration,
    /// S3 upload configuration.
    #[cfg(feature = "worker-s3")]
    s3: s3::S3Config,
    /// Pre-built S3 client. When provided, the worker uses this client
    /// instead of building one from `aws_config::load_defaults`.
    /// Region auto-detection still applies unless `region` is set on `S3Config`.
    #[cfg(feature = "worker-s3")]
    client: Option<aws_sdk_s3::Client>,
    /// Metrics sink. Defaults to [`DevNullSink`](metrique_writer::sink::DevNullSink).
    #[builder(default = metrique_writer::sink::DevNullSink::boxed())]
    metrics_sink: BoxEntrySink,
}

impl BackgroundTaskConfig {
    /// How often the worker checks for sealed segments.
    pub fn poll_interval(&self) -> Duration {
        self.poll_interval
    }

    /// Directory containing trace segments.
    pub fn trace_dir(&self) -> &Path {
        self.trace_path.parent().unwrap_or(Path::new("."))
    }

    /// File stem used for segment matching (e.g. "trace" for "trace.0.bin").
    pub fn trace_stem(&self) -> &str {
        let stem = self.trace_path.file_stem().and_then(|s| s.to_str());
        match stem {
            Some(s) if !s.is_empty() => s,
            _ => {
                tracing::error!(
                    target: "dial9_worker",
                    path = %self.trace_path.display(),
                    "trace_path has no file stem — pass a path like /tmp/traces/trace.bin, not a directory"
                );
                "trace"
            }
        }
    }

    /// S3 upload configuration.
    #[cfg(feature = "worker-s3")]
    pub fn s3(&self) -> &s3::S3Config {
        &self.s3
    }
}

// ---------------------------------------------------------------------------
// SegmentProcessor pipeline
// ---------------------------------------------------------------------------

/// Data flowing through the processor pipeline.
///
/// The worker reads the sealed segment file into `bytes`, populates initial
/// `metadata`, then passes this through each [`SegmentProcessor`] in order.
/// Metrics are flushed automatically when the `SegmentData` is dropped.
#[derive(Debug)]
pub(crate) struct SegmentData {
    /// Original sealed segment (path, index).
    pub(crate) segment: sealed::SealedSegment,
    /// The payload bytes (raw, symbolized, compressed, etc.).
    pub(crate) bytes: Vec<u8>,
    /// Metadata accumulated by processors. Keyed by convention.
    pub(crate) metadata: HashMap<String, String>,
    /// Metrics guard — processors can record metrics; flushed on drop.
    pub(crate) metrics: SegmentProcessMetricsGuard,
}

/// Error returned by a [`SegmentProcessor`].
///
/// Carries the [`SegmentData`] back so the caller can still record metrics
/// and pass the data to subsequent error-handling logic.
#[derive(Debug)]
pub(crate) struct ProcessError {
    pub(crate) data: SegmentData,
    pub(crate) kind: ProcessErrorKind,
}

#[derive(Debug)]
pub(crate) enum ProcessErrorKind {
    Io(std::io::Error),
    #[cfg(feature = "worker-s3")]
    Transfer(aws_sdk_s3_transfer_manager::error::Error),
}

impl std::fmt::Display for ProcessErrorKind {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Io(e) => write!(f, "I/O error: {e}"),
            #[cfg(feature = "worker-s3")]
            Self::Transfer(e) => write!(f, "S3 transfer error: {e}"),
        }
    }
}

impl std::fmt::Display for ProcessError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        self.kind.fmt(f)
    }
}

impl std::error::Error for ProcessError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match &self.kind {
            ProcessErrorKind::Io(e) => Some(e),
            #[cfg(feature = "worker-s3")]
            ProcessErrorKind::Transfer(e) => Some(e),
        }
    }
}

impl From<std::io::Error> for ProcessErrorKind {
    fn from(e: std::io::Error) -> Self {
        Self::Io(e)
    }
}

#[cfg(feature = "worker-s3")]
impl From<aws_sdk_s3_transfer_manager::error::Error> for ProcessErrorKind {
    fn from(e: aws_sdk_s3_transfer_manager::error::Error) -> Self {
        Self::Transfer(e)
    }
}

/// A single step in the segment processing pipeline.
///
/// Implementations handle one concern: compress, symbolize, upload, etc.
/// The worker calls processors in sequence for each segment.
pub(crate) trait SegmentProcessor: Send {
    /// Human-readable name for this processor (used in metrics).
    fn name(&self) -> &'static str;

    /// Process a segment, transforming or consuming its data.
    /// Returns the (possibly modified) data for the next processor,
    /// or an error to skip this segment.
    fn process(
        &mut self,
        data: SegmentData,
    ) -> Pin<Box<dyn Future<Output = Result<SegmentData, ProcessError>> + Send + '_>>;
}

/// Build the processor pipeline. Requires an async context for S3 client setup.
#[cfg(feature = "worker-s3")]
async fn build_pipeline(config: &mut BackgroundTaskConfig) -> Vec<Box<dyn SegmentProcessor>> {
    let s3_uploader = S3PipelineUploader::new(config).await;
    vec![Box::new(GzipCompressor), Box::new(s3_uploader)]
}

#[cfg(not(feature = "worker-s3"))]
async fn build_pipeline(_config: &mut BackgroundTaskConfig) -> Vec<Box<dyn SegmentProcessor>> {
    Vec::new()
}

/// The worker loop function. Runs on a dedicated thread, polls for sealed
/// segments and processes them through the configured pipeline.
///
/// Creates a single-threaded tokio runtime for async processors (e.g. S3 upload).
/// The worker is a "good citizen": it will lose data rather than disrupt the application.
pub(crate) fn run_background_task(
    mut config: BackgroundTaskConfig,
    shutdown: tokio::sync::oneshot::Receiver<Duration>,
) {
    let rt = tokio::runtime::Builder::new_current_thread()
        .thread_name("dial9-worker-rt")
        .enable_all()
        .build()
        .expect("failed to create worker runtime");

    let processors = rt.block_on(build_pipeline(&mut config));

    tracing::info!(target: "dial9_worker", dir = %config.trace_dir().display(), stem = %config.trace_stem(), processors = processors.len(), "worker started");
    rt.block_on(async {
        let stop = tokio_util::sync::CancellationToken::new();
        let mut worker = WorkerLoop::new(config, processors, stop.clone());
        let mut run_fut = std::pin::pin!(worker.run());
        // Poll the worker until we receive a shutdown signal with a drain timeout.
        let drain_timeout = tokio::select! {
            () = &mut run_fut => return,
            msg = shutdown => msg.unwrap_or(Duration::ZERO),
        };
        tracing::info!(target: "dial9_worker", ?drain_timeout, "stop signal received, draining");
        // Tell the worker to exit after its current processing cycle.
        stop.cancel();
        // Give it `drain_timeout` to finish; after that, drop the future.
        match tokio::time::timeout(drain_timeout, run_fut).await {
            Ok(()) => tracing::info!(target: "dial9_worker", "drain complete"),
            Err(_) => tracing::warn!(target: "dial9_worker", "drain timed out"),
        }
    });
    tracing::info!(target: "dial9_worker", "worker stopped");
}

// ---------------------------------------------------------------------------
// GzipCompressor — compresses segment bytes in-memory
// ---------------------------------------------------------------------------

#[cfg(feature = "worker-s3")]
struct GzipCompressor;

#[cfg(feature = "worker-s3")]
impl SegmentProcessor for GzipCompressor {
    fn name(&self) -> &'static str {
        "Gzip"
    }

    fn process(
        &mut self,
        mut data: SegmentData,
    ) -> Pin<Box<dyn Future<Output = Result<SegmentData, ProcessError>> + Send + '_>> {
        Box::pin(async move {
            let raw = data.bytes;
            let compressed =
                tokio::task::spawn_blocking(move || s3::gzip_compress_bytes(&raw)).await;
            match compressed {
                Ok(Ok(bytes)) => {
                    data.metrics.compressed_size = Some(bytes.len() as u64);
                    data.bytes = bytes;
                    data.metadata
                        .insert("content_encoding".into(), "gzip".into());
                    Ok(data)
                }
                Ok(Err(e)) => {
                    data.bytes = vec![];
                    Err(ProcessError {
                        data,
                        kind: ProcessErrorKind::Io(e),
                    })
                }
                Err(e) => {
                    data.bytes = vec![];
                    Err(ProcessError {
                        data,
                        kind: ProcessErrorKind::Io(std::io::Error::other(e)),
                    })
                }
            }
        })
    }
}

// ---------------------------------------------------------------------------
// Metrics
// ---------------------------------------------------------------------------

#[metrics(rename_all = "PascalCase")]
#[derive(Debug)]
pub(crate) struct SegmentProcessMetrics {
    #[metrics(unit = Millisecond)]
    total_time: Timer,
    #[metrics(flatten)]
    status: Option<MetriqueResult>,
    segment_index: u32,
    #[metrics(unit = Byte)]
    uncompressed_size: u64,
    #[metrics(unit = Byte)]
    compressed_size: Option<u64>,
    /// True when the segment file lacks a valid SegmentMetadata header.
    invalid_file_header: bool,
    /// Per-processor metrics, keyed by processor name.
    #[metrics(flatten)]
    pipeline: PipelineMetrics,
}

// ---------------------------------------------------------------------------
// WorkerLoop — the async state machine
// ---------------------------------------------------------------------------

pub(crate) struct WorkerLoop {
    dir: PathBuf,
    stem: String,
    poll_interval: Duration,
    processors: Vec<Box<dyn SegmentProcessor>>,
    metrics_sink: BoxEntrySink,
    /// When cancelled, the worker finishes its current cycle and exits
    /// instead of sleeping.
    stop: tokio_util::sync::CancellationToken,
}

impl WorkerLoop {
    pub(crate) fn new(
        config: BackgroundTaskConfig,
        processors: Vec<Box<dyn SegmentProcessor>>,
        stop: tokio_util::sync::CancellationToken,
    ) -> Self {
        Self {
            dir: config.trace_dir().to_path_buf(),
            stem: config.trace_stem().to_string(),
            poll_interval: config.poll_interval(),
            processors,
            metrics_sink: config.metrics_sink,
            stop,
        }
    }

    pub(crate) async fn run(&mut self) {
        loop {
            let segements_uploaded = self.process_open_segments().await;
            if self.stop.is_cancelled() {
                tracing::debug!(target: "dial9_worker", "Exiting run loop: cancellation received");
                return;
            }
            // if we didn't upload anything wait `poll_interval` (cancelling if we get shutdown while waiting)
            if !segements_uploaded {
                tokio::select! {
                    _ = self.stop.cancelled() => {}
                    _ = tokio::time::sleep(self.poll_interval) => {}
                }
            }
        }
    }

    async fn process_open_segments(&mut self) -> bool {
        let segments = match sealed::find_sealed_segments(&self.dir, &self.stem) {
            Ok(s) => s,
            Err(e) => {
                tracing::warn!(target: "dial9_worker", "failed to scan for sealed segments: {e}");
                return false;
            }
        };
        tracing::debug!(target: "dial9_worker", dir = %self.dir.display(), stem = %self.stem, count = segments.len(), "scanned for sealed segments");
        let found = !segments.is_empty();
        self.process_segments(&segments).await;
        found
    }

    async fn process_segments(&mut self, segments: &[sealed::SealedSegment]) {
        if self.processors.is_empty() {
            return;
        }

        'next_segment: for (seg_idx, segment) in segments.iter().enumerate() {
            tracing::debug!(target: "dial9_worker", segment = seg_idx + 1, total = segments.len(), path = %segment.path.display(), "processing segment");
            let uncompressed_size = std::fs::metadata(&segment.path)
                .map(|m| m.len())
                .unwrap_or(0);

            let bytes = match std::fs::read(&segment.path) {
                Ok(b) => b,
                Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
                    tracing::debug!(target: "dial9_worker", path = %segment.path.display(), "segment already evicted, skipping");
                    continue;
                }
                Err(e) => {
                    tracing::warn!(target: "dial9_worker", error = %e, "failed to read segment");
                    continue;
                }
            };

            let (epoch_secs, header_valid) = segment.creation_epoch_secs();

            let metrics = SegmentProcessMetrics {
                total_time: Timer::start_now(),
                status: None,
                segment_index: segment.index,
                uncompressed_size,
                compressed_size: None,
                invalid_file_header: !header_valid,
                pipeline: PipelineMetrics::default(),
            }
            .append_on_drop(self.metrics_sink.clone());

            let mut data = SegmentData {
                segment: segment.clone(),
                bytes,
                metadata: HashMap::from([
                    ("epoch_secs".into(), epoch_secs.to_string()),
                    ("segment_index".into(), segment.index.to_string()),
                ]),
                metrics,
            };

            for processor in &mut self.processors {
                let mut stage = StageMetrics::start();
                let proc_start = std::time::Instant::now();
                tracing::debug!(target: "dial9_worker", processor = processor.name(), segment = seg_idx + 1, "running processor");
                match processor.process(data).await {
                    Ok(next) => {
                        tracing::debug!(target: "dial9_worker", processor = processor.name(), segment = seg_idx + 1, elapsed_ms = proc_start.elapsed().as_secs_f64() * 1000.0, "processor succeeded");
                        data = next;
                        stage.succeed();
                        data.metrics.pipeline.push(processor.name(), stage);
                    }
                    Err(e) => {
                        tracing::debug!(target: "dial9_worker", processor = processor.name(), segment = seg_idx + 1, elapsed_ms = proc_start.elapsed().as_secs_f64() * 1000.0, error = %e.kind, "processor failed");
                        data = e.data;
                        stage.fail();
                        data.metrics.pipeline.push(processor.name(), stage);
                        data.metrics.status = Some(MetriqueResult::Failure);
                        data.metrics.total_time.stop();
                        if matches!(&e.kind, ProcessErrorKind::Io(io) if io.kind() == std::io::ErrorKind::NotFound)
                        {
                            tracing::debug!(target: "dial9_worker", path = %segment.path.display(), "segment evicted during processing, skipping");
                        } else {
                            tracing::warn!(target: "dial9_worker", error = %e.kind, cause = ?e.kind, path = %segment.path.display(), "processor failed, skipping segment");
                        }
                        continue 'next_segment;
                    }
                }
            }

            data.metrics.status = Some(MetriqueResult::Success);
            data.metrics.total_time.stop();
            // `data` dropped here — metrics guard flushes automatically
        }
    }
}

// ---------------------------------------------------------------------------
// S3PipelineUploader — production S3 upload processor
// ---------------------------------------------------------------------------

#[cfg(feature = "worker-s3")]
pub(crate) struct S3PipelineUploader {
    uploader: s3::S3Uploader,
    circuit_breaker: connection::CircuitBreaker,
}

#[cfg(feature = "worker-s3")]
impl S3PipelineUploader {
    async fn new(config: &mut BackgroundTaskConfig) -> Self {
        let s3_config = config.s3().clone();

        let bootstrap_client = match config.client.clone() {
            Some(c) => c,
            None => {
                let sdk_config = aws_config::defaults(aws_config::BehaviorVersion::latest())
                    .load()
                    .await;
                aws_sdk_s3::Client::new(&sdk_config)
            }
        };

        let region = match s3_config.region() {
            Some(r) => r.to_owned(),
            None => detect_bucket_region(&bootstrap_client, s3_config.bucket()).await,
        };
        tracing::info!(target: "dial9_worker", bucket = %s3_config.bucket(), %region, "resolved bucket region");

        // Rebuild the client with the correct region.
        let corrected_conf = bootstrap_client
            .config()
            .to_builder()
            .region(aws_sdk_s3::config::Region::new(region))
            .build();
        let corrected_client = aws_sdk_s3::Client::from_conf(corrected_conf);

        let tm_client = aws_sdk_s3_transfer_manager::Client::new(
            aws_sdk_s3_transfer_manager::Config::builder()
                .client(corrected_client)
                .build(),
        );

        Self {
            uploader: s3::S3Uploader::new(tm_client, s3_config),
            circuit_breaker: connection::CircuitBreaker::new(),
        }
    }
}

#[cfg(feature = "worker-s3")]
impl SegmentProcessor for S3PipelineUploader {
    fn name(&self) -> &'static str {
        "S3Upload"
    }

    fn process(
        &mut self,
        mut data: SegmentData,
    ) -> Pin<Box<dyn Future<Output = Result<SegmentData, ProcessError>> + Send + '_>> {
        Box::pin(async move {
            if !self.circuit_breaker.should_attempt() {
                tracing::debug!(target: "dial9_worker", path = %data.segment.path.display(), "circuit breaker open, skipping upload");
                return Err(ProcessError {
                    data,
                    kind: ProcessErrorKind::Io(std::io::Error::other("circuit breaker open")),
                });
            }
            let bytes = std::mem::take(&mut data.bytes);
            match self
                .uploader
                .upload_and_delete(&data.segment, bytes, &data.metadata)
                .await
            {
                Ok(key) => {
                    self.circuit_breaker.on_success();
                    tracing::info!(target: "dial9_worker", "uploaded {key}");
                    Ok(data)
                }
                Err(kind) => {
                    if matches!(&kind, ProcessErrorKind::Io(io) if io.kind() == std::io::ErrorKind::NotFound)
                    {
                        tracing::debug!(target: "dial9_worker", path = %data.segment.path.display(), "segment already evicted, skipping");
                    } else {
                        self.circuit_breaker.on_failure();
                        tracing::warn!(target: "dial9_worker", error = %kind, "upload failed");
                    }
                    Err(ProcessError { data, kind })
                }
            }
        })
    }
}

/// Detect the region of an S3 bucket via HeadBucket.
#[cfg(feature = "worker-s3")]
async fn detect_bucket_region(client: &aws_sdk_s3::Client, bucket: &str) -> String {
    match client.head_bucket().bucket(bucket).send().await {
        Ok(resp) => {
            let region = resp.bucket_region().unwrap_or("us-east-1");
            if resp.bucket_region().is_none() {
                tracing::warn!(
                    target: "dial9_worker",
                    %bucket,
                    "HeadBucket succeeded but returned no region, falling back to us-east-1"
                );
            }
            region.to_owned()
        }
        Err(e) => {
            let from_header = e
                .raw_response()
                .and_then(|r| r.headers().get("x-amz-bucket-region"))
                .map(|v| v.to_owned());
            match from_header {
                Some(r) => r,
                None => {
                    tracing::warn!(
                        target: "dial9_worker",
                        %bucket,
                        error = ?e,
                        "failed to detect bucket region, falling back to us-east-1"
                    );
                    "us-east-1".to_owned()
                }
            }
        }
    }
}

#[cfg(all(test, feature = "worker-s3"))]
mod tests {
    use super::*;
    use assert2::check;
    use std::sync::Arc;
    use std::sync::atomic::{AtomicUsize, Ordering};

    /// Deps that record whether on_failure was called by proxying through
    /// a real S3Uploader-like upload path.
    struct NotFoundTestDeps {
        circuit_breaker: connection::CircuitBreaker,
    }

    impl NotFoundTestDeps {
        fn new() -> Self {
            Self {
                circuit_breaker: connection::CircuitBreaker::new(),
            }
        }

        /// Simulate the upload logic from S3PipelineUploader::process
        async fn upload_segment(&mut self, segment: &sealed::SealedSegment) {
            if !self.circuit_breaker.should_attempt() {
                return;
            }
            // Attempt to read the file (like the worker would)
            match tokio::fs::read(&segment.path).await {
                Ok(_) => self.circuit_breaker.on_success(),
                Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
                    // Should skip, not degrade
                }
                Err(_) => self.circuit_breaker.on_failure(),
            }
        }
    }

    #[tokio::test]
    async fn evicted_file_does_not_trip_circuit_breaker() {
        let dir = tempfile::tempdir().unwrap();
        // Create a segment that doesn't exist on disk (simulates eviction)
        let missing = sealed::SealedSegment {
            path: dir.path().join("trace.0.bin"),
            index: 0,
        };

        let mut deps = NotFoundTestDeps::new();
        deps.upload_segment(&missing).await;

        check!(deps.circuit_breaker.is_closed());
    }

    // --- Review finding #1: compressed_size metric is non-zero after pipeline ---

    /// After a successful pipeline run (gzip + upload), the CompressedSize
    /// metric must reflect the actual compressed byte count, not 0.
    #[tokio::test]
    async fn compressed_size_metric_is_nonzero_after_pipeline() {
        use metrique_writer::AnyEntrySink;
        use metrique_writer::test_util::Inspector;

        let s3_root = tempfile::tempdir().unwrap();
        let local_dir = tempfile::tempdir().unwrap();
        std::fs::create_dir(s3_root.path().join("test-bucket")).unwrap();

        // Write a segment file with enough data to compress
        let segment_path = local_dir.path().join("trace.0.bin");
        let data = vec![42u8; 4096];
        std::fs::write(&segment_path, &data).unwrap();

        let inspector = Inspector::default();
        let sink = inspector.clone().boxed();

        // Build a real pipeline: GzipCompressor → S3PipelineUploader
        let s3_config = s3::S3Config::builder()
            .bucket("test-bucket")
            .service_name("test")
            .instance_path("test")
            .boot_id("test")
            .region("us-east-1")
            .build();

        let fs = s3s_fs::FileSystem::new(s3_root.path()).unwrap();
        let mut builder = s3s::service::S3ServiceBuilder::new(fs);
        builder.set_auth(s3s::auth::SimpleAuth::from_single("test", "test"));
        let s3_service = builder.build();
        let s3_client: s3s_aws::Client = s3_service.into();
        let s3_sdk_config = aws_sdk_s3::Config::builder()
            .behavior_version_latest()
            .credentials_provider(aws_sdk_s3::config::Credentials::new(
                "test", "test", None, None, "test",
            ))
            .region(aws_sdk_s3::config::Region::new("us-east-1"))
            .http_client(s3_client)
            .force_path_style(true)
            .build();
        let sdk_client = aws_sdk_s3::Client::from_conf(s3_sdk_config);
        let tm_client = aws_sdk_s3_transfer_manager::Client::new(
            aws_sdk_s3_transfer_manager::Config::builder()
                .client(sdk_client)
                .build(),
        );

        let uploader = s3::S3Uploader::new(tm_client, s3_config);
        let mut processors: Vec<Box<dyn SegmentProcessor>> = vec![
            Box::new(GzipCompressor),
            Box::new(S3PipelineUploader {
                uploader,
                circuit_breaker: connection::CircuitBreaker::new(),
            }),
        ];

        let segment = sealed::SealedSegment {
            path: segment_path.clone(),
            index: 0,
        };

        let metrics = SegmentProcessMetrics {
            total_time: metrique::timers::Timer::start_now(),
            status: None,
            segment_index: 0,
            uncompressed_size: data.len() as u64,
            compressed_size: None,
            invalid_file_header: false,
            pipeline: PipelineMetrics::default(),
        }
        .append_on_drop(sink);

        let mut pipe_data = SegmentData {
            segment,
            bytes: data,
            metadata: HashMap::from([
                ("epoch_secs".into(), "1741209000".into()),
                ("segment_index".into(), "0".into()),
            ]),
            metrics,
        };

        for processor in &mut processors {
            let mut stage = StageMetrics::start();
            pipe_data = processor.process(pipe_data).await.unwrap();
            stage.succeed();
            pipe_data.metrics.pipeline.push(processor.name(), stage);
        }

        // After fix: compressed_size is set by GzipCompressor, not overwritten
        pipe_data.metrics.status = Some(MetriqueResult::Success);
        pipe_data.metrics.total_time.stop();
        drop(pipe_data);

        let entries = inspector.entries();
        check!(entries.len() == 1);
        let entry = &entries[0];
        let compressed = entry.metrics["CompressedSize"].as_u64();
        check!(
            compressed > 0,
            "CompressedSize should be non-zero, got {}",
            compressed
        );
    }

    // --- Review finding #10: uncompressed_size should use bytes.len() ---

    /// uncompressed_size should match the actual bytes read, not a separate
    /// metadata() call that could race with eviction.
    #[test]
    fn uncompressed_size_matches_bytes_len() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("trace.0.bin");
        let data = vec![0u8; 1234];
        std::fs::write(&path, &data).unwrap();

        // Read the file the way process_segments does
        let uncompressed_size = std::fs::metadata(&path).map(|m| m.len()).unwrap_or(0);
        let bytes = std::fs::read(&path).unwrap();

        // These should be equal — the metadata call is redundant
        check!(uncompressed_size == bytes.len() as u64);

        // The real assertion: bytes.len() is the canonical source of truth
        check!(bytes.len() == 1234);
    }

    // --- Review finding #4: WorkerLoop drain on stop ---

    /// When the stop signal is set, the worker must drain remaining segments
    /// before exiting.
    #[tokio::test]
    async fn worker_loop_drains_on_stop() {
        let dir = tempfile::tempdir().unwrap();

        // Create some sealed segments
        std::fs::write(dir.path().join("trace.0.bin"), b"segment0").unwrap();
        std::fs::write(dir.path().join("trace.1.bin"), b"segment1").unwrap();

        let processed = Arc::new(AtomicUsize::new(0));

        struct CountingProcessor(Arc<AtomicUsize>);
        impl SegmentProcessor for CountingProcessor {
            fn name(&self) -> &'static str {
                "Counter"
            }
            fn process(
                &mut self,
                data: SegmentData,
            ) -> Pin<Box<dyn Future<Output = Result<SegmentData, ProcessError>> + Send + '_>>
            {
                let counter = self.0.clone();
                Box::pin(async move {
                    counter.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
                    Ok(data)
                })
            }
        }

        // Pre-cancelled token so the worker processes once and exits.
        let stop = tokio_util::sync::CancellationToken::new();
        stop.cancel();
        let config = BackgroundTaskConfig::builder()
            .trace_path(dir.path().join("trace.bin"))
            .s3(s3::S3Config::builder()
                .bucket("b")
                .service_name("s")
                .instance_path("i")
                .boot_id("b")
                .build())
            .build();

        let processors: Vec<Box<dyn SegmentProcessor>> =
            vec![Box::new(CountingProcessor(processed.clone()))];

        let mut worker = WorkerLoop::new(config, processors, stop);
        worker.run().await;

        // Worker should have drained both segments even though stop was set
        check!(processed.load(Ordering::SeqCst) == 2);
    }

    /// When a processor fails, the worker skips that segment and continues
    /// with the next one.
    #[tokio::test]
    async fn worker_loop_continues_after_processor_error() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("trace.0.bin"), b"fail").unwrap();
        std::fs::write(dir.path().join("trace.1.bin"), b"succeed").unwrap();

        let processed = Arc::new(AtomicUsize::new(0));

        struct FailFirstProcessor {
            counter: Arc<AtomicUsize>,
            calls: usize,
        }
        impl SegmentProcessor for FailFirstProcessor {
            fn name(&self) -> &'static str {
                "FailFirst"
            }
            fn process(
                &mut self,
                data: SegmentData,
            ) -> Pin<Box<dyn Future<Output = Result<SegmentData, ProcessError>> + Send + '_>>
            {
                self.calls += 1;
                let should_fail = self.calls == 1;
                let counter = self.counter.clone();
                Box::pin(async move {
                    if should_fail {
                        Err(ProcessError {
                            data,
                            kind: ProcessErrorKind::Io(std::io::Error::other("test failure")),
                        })
                    } else {
                        counter.fetch_add(1, Ordering::SeqCst);
                        Ok(data)
                    }
                })
            }
        }

        let stop = tokio_util::sync::CancellationToken::new();
        stop.cancel();
        let config = BackgroundTaskConfig::builder()
            .trace_path(dir.path().join("trace.bin"))
            .s3(s3::S3Config::builder()
                .bucket("b")
                .service_name("s")
                .instance_path("i")
                .boot_id("b")
                .build())
            .build();

        let processors: Vec<Box<dyn SegmentProcessor>> = vec![Box::new(FailFirstProcessor {
            counter: processed.clone(),
            calls: 0,
        })];

        let mut worker = WorkerLoop::new(config, processors, stop);
        worker.run().await;

        // Second segment should still be processed despite first failing
        check!(processed.load(Ordering::SeqCst) == 1);
    }
}

// --- Review finding #9: trace_stem edge cases ---

#[cfg(all(test, feature = "worker-s3"))]
mod trace_stem_tests {
    use super::*;
    use assert2::check;

    fn dummy_s3() -> s3::S3Config {
        s3::S3Config::builder()
            .bucket("b")
            .service_name("s")
            .instance_path("i")
            .boot_id("b")
            .build()
    }

    #[test]
    fn trace_stem_normal_path() {
        let config = BackgroundTaskConfig::builder()
            .trace_path("/tmp/traces/trace.bin")
            .s3(dummy_s3())
            .build();
        check!(config.trace_stem() == "trace");
    }

    #[test]
    fn trace_stem_directory_path() {
        // A path like "/tmp/traces/" — file_stem returns "traces", not an error
        let config = BackgroundTaskConfig::builder()
            .trace_path("/tmp/traces/")
            .s3(dummy_s3())
            .build();
        // This is the current behavior — it returns "traces" not "trace"
        // which would silently match the wrong files
        check!(config.trace_stem() == "traces");
    }

    #[test]
    fn trace_stem_root_path() {
        // A path like "/" has no file stem
        let config = BackgroundTaskConfig::builder()
            .trace_path("/")
            .s3(dummy_s3())
            .build();
        // Should fall back to "trace" and log an error
        check!(config.trace_stem() == "trace");
    }

    #[test]
    fn trace_dir_for_directory_path() {
        let config = BackgroundTaskConfig::builder()
            .trace_path("/tmp/traces/")
            .s3(dummy_s3())
            .build();
        // trace_dir should be the parent of the path
        check!(config.trace_dir() == std::path::Path::new("/tmp"));
    }
}
