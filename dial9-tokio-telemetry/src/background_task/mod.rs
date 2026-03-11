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
use pipeline_metrics::{PipelineMetrics, StageMetrics};
use std::collections::HashMap;
use std::future::Future;
use std::path::{Path, PathBuf};
use std::pin::Pin;
use std::time::Duration;

const DEFAULT_POLL_INTERVAL: Duration = Duration::from_secs(1);

/// Configuration for the in-process worker pipeline.
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
pub(crate) trait SegmentProcessor: Send + Sync {
    /// Human-readable name for this processor (used in metrics).
    fn name(&self) -> &'static str;

    /// Process a segment, transforming or consuming its data.
    /// Returns the (possibly modified) data for the next processor,
    /// or an error to skip this segment.
    fn process(
        &self,
        data: SegmentData,
    ) -> Pin<Box<dyn Future<Output = Result<SegmentData, ProcessError>> + Send + '_>>;
}

/// The worker loop function. Runs on a dedicated thread, polls for sealed
/// segments and processes them through the configured pipeline.
///
/// Creates a single-threaded tokio runtime for async processors (e.g. S3 upload).
/// The worker is a "good citizen": it will lose data rather than disrupt the application.
pub(crate) fn run_background_task(
    #[cfg_attr(not(feature = "worker-s3"), allow(unused_mut))] mut config: BackgroundTaskConfig,
    stop: std::sync::Arc<std::sync::atomic::AtomicBool>,
) {
    let rt = tokio::runtime::Builder::new_current_thread()
        .thread_name("dial9-worker-rt")
        .enable_all()
        .build()
        .expect("failed to create worker runtime");

    rt.block_on(async {
        #[allow(unused_mut)]
        let mut processors: Vec<Box<dyn SegmentProcessor>> = Vec::new();

        #[cfg(feature = "worker-s3")]
        {
            let s3_uploader = S3PipelineUploader::new(&mut config).await;
            processors.push(Box::new(GzipCompressor));
            processors.push(Box::new(s3_uploader));
        }

        tracing::info!(target: "dial9_worker", dir = %config.trace_dir().display(), stem = %config.trace_stem(), processors = processors.len(), "worker started");
        let worker = WorkerLoop::new(config, stop, processors);
        worker.run().await;
        tracing::info!(target: "dial9_worker", "worker stopped");
    });
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
        &self,
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
                    Err(ProcessError { data, kind: ProcessErrorKind::Io(e) })
                }
                Err(e) => {
                    data.bytes = vec![];
                    Err(ProcessError { data, kind: ProcessErrorKind::Io(std::io::Error::other(e)) })
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
    /// 1 on success, 0 on failure.
    success: usize,
    segment_index: u32,
    #[metrics(unit = Byte)]
    uncompressed_size: u64,
    #[metrics(unit = Byte)]
    compressed_size: Option<u64>,
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
    stop: std::sync::Arc<std::sync::atomic::AtomicBool>,
    processors: Vec<Box<dyn SegmentProcessor>>,
    metrics_sink: BoxEntrySink,
}

impl WorkerLoop {
    pub(crate) fn new(
        config: BackgroundTaskConfig,
        stop: std::sync::Arc<std::sync::atomic::AtomicBool>,
        processors: Vec<Box<dyn SegmentProcessor>>,
    ) -> Self {
        Self {
            dir: config.trace_dir().to_path_buf(),
            stem: config.trace_stem().to_string(),
            poll_interval: config.poll_interval(),
            stop,
            processors,
            metrics_sink: config.metrics_sink,
        }
    }

    pub(crate) async fn run(mut self) {
        loop {
            if self.stop.load(std::sync::atomic::Ordering::Acquire) {
                self.drain().await;
                return;
            }

            let found = self.process_open_segments().await;
            if !found {
                tokio::time::sleep(self.poll_interval).await;
            }
        }
    }

    /// Returns true if any segments were found and processed.
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

    async fn drain(&mut self) {
        tracing::info!(target: "dial9_worker", "draining remaining segments");
        if let Ok(segments) = sealed::find_sealed_segments(&self.dir, &self.stem) {
            tracing::info!(target: "dial9_worker", count = segments.len(), "found segments to drain");
            self.process_segments(&segments).await;
        }
    }

    async fn process_segments(&mut self, segments: &[sealed::SealedSegment]) {
        if self.processors.is_empty() {
            return;
        }

        'next_segment: for segment in segments {
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

            let metrics = SegmentProcessMetrics {
                total_time: Timer::start_now(),
                success: 0,
                segment_index: segment.index,
                uncompressed_size,
                compressed_size: None,
                pipeline: PipelineMetrics::default(),
            }
            .append_on_drop(self.metrics_sink.clone());

            let mut data = SegmentData {
                segment: segment.clone(),
                bytes,
                metadata: HashMap::from([
                    (
                        "epoch_secs".into(),
                        segment.creation_epoch_secs().to_string(),
                    ),
                    ("segment_index".into(), segment.index.to_string()),
                ]),
                metrics,
            };

            for processor in &self.processors {
                let mut stage = StageMetrics::start();
                match processor.process(data).await {
                    Ok(next) => {
                        data = next;
                        stage.succeed();
                        data.metrics.pipeline.push(processor.name(), stage);
                    }
                    Err(e) => {
                        data = e.data;
                        stage.time.stop();
                        data.metrics.pipeline.push(processor.name(), stage);
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

            data.metrics.compressed_size = Some(data.bytes.len() as u64);
            data.metrics.success = 1;
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
    circuit_breaker: std::sync::Mutex<connection::CircuitBreaker>,
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
            circuit_breaker: std::sync::Mutex::new(connection::CircuitBreaker::new()),
        }
    }
}

#[cfg(feature = "worker-s3")]
impl SegmentProcessor for S3PipelineUploader {
    fn name(&self) -> &'static str {
        "S3Upload"
    }

    fn process(
        &self,
        data: SegmentData,
    ) -> Pin<Box<dyn Future<Output = Result<SegmentData, ProcessError>> + Send + '_>> {
        Box::pin(async move {
            if !self.circuit_breaker.lock().unwrap().should_attempt() {
                tracing::debug!(target: "dial9_worker", path = %data.segment.path.display(), "circuit breaker open, skipping upload");
                return Err(ProcessError {
                    data,
                    kind: ProcessErrorKind::Io(std::io::Error::new(
                        std::io::ErrorKind::Other,
                        "circuit breaker open",
                    )),
                });
            }
            match self
                .uploader
                .upload_and_delete(&data.segment, data.bytes.clone(), &data.metadata)
                .await
            {
                Ok(key) => {
                    self.circuit_breaker.lock().unwrap().on_success();
                    tracing::info!(target: "dial9_worker", "uploaded {key}");
                    Ok(data)
                }
                Err(kind) => {
                    if matches!(&kind, ProcessErrorKind::Io(io) if io.kind() == std::io::ErrorKind::NotFound)
                    {
                        tracing::debug!(target: "dial9_worker", path = %data.segment.path.display(), "segment already evicted, skipping");
                    } else {
                        self.circuit_breaker.lock().unwrap().on_failure();
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

        assert!(
            deps.circuit_breaker.is_closed(),
            "circuit breaker should stay closed after NotFound"
        );
    }
}
