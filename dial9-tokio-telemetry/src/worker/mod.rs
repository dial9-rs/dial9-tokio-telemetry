pub mod sealed;
pub mod identity;
#[cfg(feature = "worker-s3")]
pub mod connection;
#[cfg(feature = "worker-s3")]
pub mod s3;

use std::path::{Path, PathBuf};
use std::time::Duration;

const DEFAULT_POLL_INTERVAL: Duration = Duration::from_secs(1);

/// Configuration for the in-process worker pipeline.
#[derive(bon::Builder)]
#[builder(on(String, into))]
pub struct WorkerConfig {
    /// The trace base path (same path passed to `RotatingWriter::new`).
    trace_path: PathBuf,
    /// How often the worker checks for sealed segments. Defaults to 1 second.
    #[builder(default = DEFAULT_POLL_INTERVAL)]
    poll_interval: Duration,
    /// S3 upload configuration.
    #[cfg(feature = "worker-s3")]
    s3: s3::S3Config,
    /// Pre-built S3 transfer manager client. When provided, the worker uses
    /// this client directly instead of building one from `aws_config::load_defaults`.
    #[cfg(feature = "worker-s3")]
    client: Option<aws_sdk_s3_transfer_manager::Client>,
}

impl WorkerConfig {
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
        self.trace_path
            .file_stem()
            .and_then(|s| s.to_str())
            .unwrap_or("trace")
    }

    /// S3 upload configuration.
    #[cfg(feature = "worker-s3")]
    pub fn s3(&self) -> &s3::S3Config {
        &self.s3
    }

    /// Take the pre-built S3 transfer manager client, if any.
    #[cfg(feature = "worker-s3")]
    pub(crate) fn take_client(&mut self) -> Option<aws_sdk_s3_transfer_manager::Client> {
        self.client.take()
    }
}

/// The worker loop function. Runs on a dedicated thread, polls for sealed
/// segments and processes them (upload to S3 if configured).
///
/// Creates a current-thread tokio runtime and runs the async worker loop inside it.
pub(crate) fn run_worker(
    #[cfg_attr(not(feature = "worker-s3"), allow(unused_mut))]
    mut config: WorkerConfig,
    stop: std::sync::Arc<std::sync::atomic::AtomicBool>,
) {
    let rt = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .expect("failed to create worker runtime");

    #[cfg(feature = "worker-s3")]
    {
        let deps = RealWorkerDeps::new(&mut config, &rt);
        let worker = WorkerLoop::new(config, stop, deps);
        rt.block_on(worker.run());
    }

    #[cfg(not(feature = "worker-s3"))]
    {
        let deps = NoOpDeps;
        let worker = WorkerLoop::new(config, stop, deps);
        rt.block_on(worker.run());
    }
}

// ---------------------------------------------------------------------------
// WorkerDeps trait — injectable for testing
// ---------------------------------------------------------------------------

/// Abstraction over the worker's external dependencies: finding segments,
/// uploading them, generating timestamps, and sleeping between polls.
pub(crate) trait WorkerDeps {
    fn find_sealed_segments(
        &self,
        dir: &Path,
        stem: &str,
    ) -> std::io::Result<Vec<sealed::SealedSegment>>;

    /// Process a segment (e.g. upload to S3).
    fn upload(
        &mut self,
        segment: &sealed::SealedSegment,
    ) -> impl std::future::Future<Output = ()>;

    fn sleep(
        &self,
        duration: Duration,
    ) -> impl std::future::Future<Output = ()>;
}

// ---------------------------------------------------------------------------
// WorkerLoop — the async state machine
// ---------------------------------------------------------------------------

pub(crate) struct WorkerLoop<D> {
    dir: PathBuf,
    stem: String,
    poll_interval: Duration,
    stop: std::sync::Arc<std::sync::atomic::AtomicBool>,
    deps: D,
}

impl<D: WorkerDeps> WorkerLoop<D> {
    pub(crate) fn new(
        config: WorkerConfig,
        stop: std::sync::Arc<std::sync::atomic::AtomicBool>,
        deps: D,
    ) -> Self {
        Self {
            dir: config.trace_dir().to_path_buf(),
            stem: config.trace_stem().to_string(),
            poll_interval: config.poll_interval(),
            stop,
            deps,
        }
    }

    pub(crate) async fn run(mut self) {
        loop {
            if self.stop.load(std::sync::atomic::Ordering::Acquire) {
                self.drain().await;
                return;
            }

            self.poll_once().await;
            self.deps.sleep(self.poll_interval).await;
        }
    }

    async fn poll_once(&mut self) {
        let segments = match self.deps.find_sealed_segments(&self.dir, &self.stem) {
            Ok(s) => s,
            Err(e) => {
                tracing::warn!(target: "dial9_worker", "failed to scan for sealed segments: {e}");
                return;
            }
        };
        for segment in &segments {
            self.deps.upload(segment).await;
        }
    }

    async fn drain(&mut self) {
        if let Ok(segments) = self.deps.find_sealed_segments(&self.dir, &self.stem) {
            for segment in &segments {
                self.deps.upload(segment).await;
            }
        }
    }
}

// ---------------------------------------------------------------------------
// NoOpDeps — used when worker-s3 is disabled
// ---------------------------------------------------------------------------

#[cfg(not(feature = "worker-s3"))]
struct NoOpDeps;

#[cfg(not(feature = "worker-s3"))]
impl WorkerDeps for NoOpDeps {
    fn find_sealed_segments(
        &self,
        dir: &Path,
        stem: &str,
    ) -> std::io::Result<Vec<sealed::SealedSegment>> {
        sealed::find_sealed_segments(dir, stem)
    }

    async fn upload(&mut self, _segment: &sealed::SealedSegment) {}

    async fn sleep(&self, duration: Duration) {
        tokio::time::sleep(duration).await;
    }
}

// ---------------------------------------------------------------------------
// RealWorkerDeps — production impl with S3
// ---------------------------------------------------------------------------

#[cfg(feature = "worker-s3")]
pub(crate) struct RealWorkerDeps {
    uploader: s3::S3Uploader,
    connection: connection::S3ConnectionState,
}

#[cfg(feature = "worker-s3")]
impl RealWorkerDeps {
    fn new(config: &mut WorkerConfig, rt: &tokio::runtime::Runtime) -> Self {
        let s3_config = config.s3().clone();

        let client = if let Some(client) = config.take_client() {
            client
        } else {
            let sdk_config =
                rt.block_on(aws_config::load_defaults(aws_config::BehaviorVersion::latest()));
            aws_sdk_s3_transfer_manager::Client::new(
                aws_sdk_s3_transfer_manager::Config::builder()
                    .client(aws_sdk_s3::Client::new(&sdk_config))
                    .build(),
            )
        };

        Self {
            uploader: s3::S3Uploader::new(client, s3_config),
            connection: connection::S3ConnectionState::healthy(),
        }
    }
}

#[cfg(feature = "worker-s3")]
impl WorkerDeps for RealWorkerDeps {
    fn find_sealed_segments(
        &self,
        dir: &Path,
        stem: &str,
    ) -> std::io::Result<Vec<sealed::SealedSegment>> {
        sealed::find_sealed_segments(dir, stem)
    }

    async fn upload(&mut self, segment: &sealed::SealedSegment) {
        if !self.connection.should_attempt_upload() {
            return;
        }
        let epoch_secs = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap_or_default()
            .as_secs();
        let timestamp = epoch_secs.to_string();
        match self.uploader.upload_and_delete(segment, &timestamp).await {
            Ok(key) => {
                self.connection.on_success();
                tracing::info!(target: "dial9_worker", "uploaded {key}");
            }
            Err(e) => {
                self.connection.on_failure();
                tracing::warn!(target: "dial9_worker", "upload failed: {e}");
            }
        }
    }

    async fn sleep(&self, duration: Duration) {
        tokio::time::sleep(duration).await;
    }
}
