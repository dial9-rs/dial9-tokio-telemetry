#[cfg(feature = "worker-s3")]
pub mod connection;
pub mod instance_metadata;
#[cfg(feature = "worker-s3")]
pub mod s3;
pub(crate) mod sealed;

use metrique::timers::Timer;
use metrique::unit::Byte;
use metrique::unit::Millisecond;
use metrique::unit_of_work::metrics;
use metrique_writer::BoxEntrySink;
use std::path::{Path, PathBuf};
use std::sync::Arc;
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

/// The worker loop function. Runs on a dedicated thread, polls for sealed
/// segments and processes them (upload to S3 if configured).
///
/// Creates a multi-threaded tokio runtime (2 worker threads) to overlap
/// compression and upload. The worker is a "good citizen": it will lose
/// data rather than disrupt the application.
pub(crate) fn run_background_task(
    #[cfg_attr(not(feature = "worker-s3"), allow(unused_mut))] mut config: BackgroundTaskConfig,
    stop: std::sync::Arc<std::sync::atomic::AtomicBool>,
) {
    let rt = tokio::runtime::Builder::new_multi_thread()
        .worker_threads(2)
        .thread_name("dial9-worker-rt")
        .enable_all()
        .build()
        .expect("failed to create worker runtime");

    rt.block_on(async {
        #[cfg(feature = "worker-s3")]
        let uploader = Some(S3Uploader::new(&mut config).await);

        #[cfg(not(feature = "worker-s3"))]
        let uploader: Option<NoUploader> = None;

        tracing::info!(target: "dial9_worker", dir = %config.trace_dir().display(), stem = %config.trace_stem(), "worker started");
        let worker = WorkerLoop::new(config, stop, uploader);
        worker.run().await;
        tracing::info!(target: "dial9_worker", "worker stopped");
    });
}

// ---------------------------------------------------------------------------
// SegmentUploader trait — pluggable upload strategy
// ---------------------------------------------------------------------------

/// Trait for uploading sealed trace segments to a remote store.
///
/// The worker loop always scans for segments and handles sleeping between
/// polls. This trait controls only what happens to each discovered segment.
pub(crate) trait SegmentUploader: Send {
    /// Compress a segment, returning opaque compressed data.
    /// This may be called from `spawn_blocking`.
    fn compress(
        &self,
        segment: &sealed::SealedSegment,
    ) -> impl std::future::Future<Output = Option<CompressedSegment>> + Send;

    /// Upload previously compressed data and delete the local file.
    fn upload_compressed(
        &self,
        segment: &sealed::SealedSegment,
        compressed: CompressedSegment,
    ) -> impl std::future::Future<Output = ()> + Send;
}

/// Opaque compressed segment data ready for upload.
pub(crate) struct CompressedSegment {
    pub(crate) data: Vec<u8>,
    pub(crate) epoch_secs: u64,
}

/// Uninhabited type used as the type parameter for `Option<T>` when no
/// uploader feature is enabled. `Option<NoUploader>` is always `None`,
/// and the compiler can optimize away the match arm.
#[cfg(not(feature = "worker-s3"))]
enum NoUploader {}

#[cfg(not(feature = "worker-s3"))]
impl SegmentUploader for NoUploader {
    async fn compress(&self, _segment: &sealed::SealedSegment) -> Option<CompressedSegment> {
        unreachable!()
    }
    async fn upload_compressed(
        &self,
        _segment: &sealed::SealedSegment,
        _compressed: CompressedSegment,
    ) {
        unreachable!()
    }
}

// ---------------------------------------------------------------------------
// Metrics
// ---------------------------------------------------------------------------

#[metrics(rename_all = "PascalCase")]
struct SegmentUploadMetrics {
    #[metrics(unit = Millisecond)]
    compression_time: Timer,
    #[metrics(unit = Millisecond)]
    upload_time: Timer,
    /// 1 on success, 0 on failure.
    success: usize,
    segment_index: u32,
    #[metrics(unit = Byte)]
    uncompressed_size: u64,
    #[metrics(unit = Byte)]
    compressed_size: u64,
}

// ---------------------------------------------------------------------------
// WorkerLoop — the async state machine
// ---------------------------------------------------------------------------

pub(crate) struct WorkerLoop<U> {
    dir: PathBuf,
    stem: String,
    poll_interval: Duration,
    stop: std::sync::Arc<std::sync::atomic::AtomicBool>,
    uploader: Option<Arc<U>>,
    metrics_sink: BoxEntrySink,
}

impl<U: SegmentUploader + Sync + 'static> WorkerLoop<U> {
    pub(crate) fn new(
        config: BackgroundTaskConfig,
        stop: std::sync::Arc<std::sync::atomic::AtomicBool>,
        uploader: Option<U>,
    ) -> Self {
        Self {
            dir: config.trace_dir().to_path_buf(),
            stem: config.trace_stem().to_string(),
            poll_interval: config.poll_interval(),
            stop,
            uploader: uploader.map(Arc::new),
            metrics_sink: config.metrics_sink,
        }
    }

    pub(crate) async fn run(mut self) {
        loop {
            if self.stop.load(std::sync::atomic::Ordering::Acquire) {
                self.drain().await;
                return;
            }

            self.poll_once().await;
            tokio::time::sleep(self.poll_interval).await;
        }
    }

    async fn poll_once(&mut self) {
        let segments = match sealed::find_sealed_segments(&self.dir, &self.stem) {
            Ok(s) => s,
            Err(e) => {
                tracing::warn!(target: "dial9_worker", "failed to scan for sealed segments: {e}");
                return;
            }
        };
        tracing::debug!(target: "dial9_worker", dir = %self.dir.display(), stem = %self.stem, count = segments.len(), "scanned for sealed segments");
        self.process_segments(&segments).await;
    }

    async fn drain(&mut self) {
        tracing::info!(target: "dial9_worker", "draining remaining segments");
        if let Ok(segments) = sealed::find_sealed_segments(&self.dir, &self.stem) {
            tracing::info!(target: "dial9_worker", count = segments.len(), "found segments to drain");
            self.process_segments(&segments).await;
        }
    }

    async fn process_segments(&mut self, segments: &[sealed::SealedSegment]) {
        let Some(uploader) = &self.uploader else {
            return;
        };

        // Compress sequentially, spawn uploads concurrently (bounded by semaphore).
        let upload_semaphore = Arc::new(tokio::sync::Semaphore::new(2));
        let mut upload_handles = Vec::new();

        for segment in segments {
            let uncompressed_size = std::fs::metadata(&segment.path)
                .map(|m| m.len())
                .unwrap_or(0);

            let mut metrics = SegmentUploadMetrics {
                compression_time: Timer::start_now(),
                upload_time: Timer::default(),
                success: 0,
                segment_index: segment.index,
                uncompressed_size,
                compressed_size: 0,
            }
            .append_on_drop(self.metrics_sink.clone());

            let compressed = uploader.compress(segment).await;
            metrics.compression_time.stop();

            let Some(c) = compressed else { continue };
            metrics.compressed_size = c.data.len() as u64;

            // Spawn upload — acquire semaphore to bound concurrency.
            let permit = upload_semaphore.clone().acquire_owned().await.unwrap();
            let uploader = Arc::clone(uploader);
            let seg = sealed::SealedSegment {
                path: segment.path.clone(),
                index: segment.index,
            };
            upload_handles.push(tokio::spawn(async move {
                metrics.upload_time = Timer::start_now();
                uploader.upload_compressed(&seg, c).await;
                metrics.upload_time.stop();
                metrics.success = 1;
                drop(permit);
            }));
        }

        for handle in upload_handles {
            let _ = handle.await;
        }
    }
}

// ---------------------------------------------------------------------------
// S3Uploader — production impl with S3
// ---------------------------------------------------------------------------

#[cfg(feature = "worker-s3")]
pub(crate) struct S3Uploader {
    uploader: s3::S3Uploader,
    circuit_breaker: std::sync::Mutex<connection::CircuitBreaker>,
}

#[cfg(feature = "worker-s3")]
impl S3Uploader {
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

/// Detect the region of an S3 bucket via HeadBucket.
///
/// On success the response includes the region directly. On a 301 redirect
/// (wrong region) the `x-amz-bucket-region` header contains the correct one.
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

#[cfg(feature = "worker-s3")]
impl SegmentUploader for S3Uploader {
    async fn compress(&self, segment: &sealed::SealedSegment) -> Option<CompressedSegment> {
        let epoch_secs = segment.creation_epoch_secs();
        let path = segment.path.clone();
        match tokio::task::spawn_blocking(move || s3::gzip_compress_file_sync(&path)).await {
            Ok(Ok(data)) => Some(CompressedSegment { data, epoch_secs }),
            Ok(Err(e)) if e.kind() == std::io::ErrorKind::NotFound => {
                tracing::debug!(target: "dial9_worker", path = %segment.path.display(), "segment already evicted, skipping");
                None
            }
            Ok(Err(e)) => {
                tracing::warn!(target: "dial9_worker", error = %e, "compression failed");
                None
            }
            Err(e) => {
                tracing::warn!(target: "dial9_worker", error = %e, "compression task panicked");
                None
            }
        }
    }

    async fn upload_compressed(
        &self,
        segment: &sealed::SealedSegment,
        compressed: CompressedSegment,
    ) {
        if !self.circuit_breaker.lock().unwrap().should_attempt() {
            return;
        }
        match self
            .uploader
            .upload_compressed_and_delete(segment, compressed.data, compressed.epoch_secs)
            .await
        {
            Ok(key) => {
                self.circuit_breaker.lock().unwrap().on_success();
                tracing::info!(target: "dial9_worker", "uploaded {key}");
            }
            Err(e) => {
                if matches!(&e, s3::UploadError::Io(io) if io.kind() == std::io::ErrorKind::NotFound)
                {
                    tracing::debug!(target: "dial9_worker", path = %segment.path.display(), "segment already evicted, skipping");
                    return;
                }
                self.circuit_breaker.lock().unwrap().on_failure();
                tracing::warn!(target: "dial9_worker", error = %e, cause = ?e, "upload failed");
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

        /// Simulate the upload logic from S3Uploader::upload
        async fn upload_segment(&mut self, segment: &sealed::SealedSegment) {
            if !self.circuit_breaker.should_attempt() {
                return;
            }
            // Attempt to read the file (like gzip_compress_file_sync would)
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
