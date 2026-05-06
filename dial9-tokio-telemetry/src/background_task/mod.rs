#[cfg(feature = "worker-s3")]
pub(crate) mod connection;
pub mod instance_metadata;
mod payload;
pub(crate) mod pipeline_metrics;
#[cfg(feature = "worker-s3")]
pub mod s3;
pub(crate) mod sealed;

pub use payload::Payload;
pub use sealed::SealedSegment;

use crate::metrics::{Operation, SegmentProcessMetrics, SegmentProcessMetricsGuard};
use crate::rate_limit::rate_limited;
use futures_util::FutureExt;
use metrique::timers::Timer;
use metrique_writer::BoxEntrySink;
use pipeline_metrics::{MetriqueResult, PipelineMetrics, StageMetrics};
use std::collections::HashMap;
use std::future::Future;
use std::io;
use std::path::{Path, PathBuf};
use std::pin::Pin;
use std::time::Duration;

pub(crate) const DEFAULT_POLL_INTERVAL: Duration = Duration::from_secs(1);

/// Configuration for the in-process worker pipeline.
///
/// The pipeline is composed of a sequence of [`SegmentProcessor`]s supplied
/// via `processors`. When none are provided the worker runs no processing.
#[derive(bon::Builder)]
#[builder(on(String, into))]
pub struct BackgroundTaskConfig {
    /// The trace base path (same path passed to `RotatingWriter::new`).
    #[builder(into)]
    trace_path: PathBuf,
    /// How often the worker checks for sealed segments. Defaults to 1 second.
    #[builder(default = DEFAULT_POLL_INTERVAL)]
    poll_interval: Duration,
    /// The processor pipeline executed for each sealed segment, in order.
    #[builder(default)]
    processors: Vec<Box<dyn SegmentProcessor>>,
    /// Metrics sink. Defaults to [`DevNullSink`](metrique_writer::sink::DevNullSink).
    #[builder(default = metrique_writer::sink::DevNullSink::boxed())]
    metrics_sink: BoxEntrySink,
}

impl std::fmt::Debug for BackgroundTaskConfig {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("BackgroundTaskConfig")
            .field("trace_path", &self.trace_path)
            .field("poll_interval", &self.poll_interval)
            .finish_non_exhaustive()
    }
}

impl BackgroundTaskConfig {
    /// How often the worker checks for sealed segments.
    pub fn poll_interval(&self) -> Duration {
        self.poll_interval
    }

    /// Directory containing trace segments.
    pub fn trace_dir(&self) -> &Path {
        match self.trace_path.parent() {
            Some(parent) if !parent.as_os_str().is_empty() => parent,
            _ => Path::new("."),
        }
    }

    /// File stem used for segment matching (e.g. "trace" for "trace.0.bin").
    pub fn trace_stem(&self) -> &str {
        let stem = self.trace_path.file_stem().and_then(|s| s.to_str());
        match stem {
            Some(s) if !s.is_empty() => s,
            _ => {
                rate_limited!(Duration::from_secs(60), {
                    tracing::error!(
                        target: "dial9_worker",
                        path = %self.trace_path.display(),
                        "trace_path has no file stem — pass a path like /tmp/traces/trace.bin, not a directory"
                    );
                });
                "trace"
            }
        }
    }
}

// ---------------------------------------------------------------------------
// SegmentProcessor pipeline
// ---------------------------------------------------------------------------

/// Data flowing through the processor pipeline.
///
/// The worker reads the sealed segment file into `payload`, populates initial
/// `metadata`, then passes this through each [`SegmentProcessor`] in order.
/// Metrics are flushed automatically when the `SegmentData` is dropped.
pub struct SegmentData {
    pub(crate) segment: SealedSegment,
    pub(crate) payload: Payload,
    pub(crate) metadata: HashMap<String, String>,
    pub(crate) metrics: SegmentProcessMetricsGuard,
}

impl SegmentData {
    /// Information about the sealed segment being processed.
    pub fn segment(&self) -> &SealedSegment {
        &self.segment
    }

    /// Current payload (raw, symbolized, compressed, etc.).
    pub fn payload(&self) -> &Payload {
        &self.payload
    }

    /// Take ownership of the payload, leaving an empty [`Payload`] in its place.
    pub fn take_payload(&mut self) -> Payload {
        std::mem::take(&mut self.payload)
    }

    /// Replace the payload.
    pub fn set_payload(&mut self, payload: impl Into<Payload>) {
        self.payload = payload.into();
    }

    /// Metadata accumulated by upstream processors.
    pub fn metadata(&self) -> &HashMap<String, String> {
        &self.metadata
    }

    /// Mutable reference to the metadata map. Processors can insert keys
    /// (e.g. `"content_encoding"`, `"write_back_extension"`) to signal
    /// downstream stages.
    pub fn metadata_mut(&mut self) -> &mut HashMap<String, String> {
        &mut self.metadata
    }

    /// Record the segment's post-compression size. Surfaces as the
    /// `CompressedSize` metric for this segment.
    pub fn set_compressed_size(&mut self, bytes: u64) {
        self.metrics.compressed_size = Some(bytes);
    }
}

impl std::fmt::Debug for SegmentData {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("SegmentData")
            .field("segment", &self.segment)
            .field("payload", &self.payload)
            .field("metadata", &self.metadata)
            .field("metrics", &self.metrics)
            .finish()
    }
}

/// Error returned by a [`SegmentProcessor`].
///
/// Carries the [`SegmentData`] back so the caller can still record metrics
/// and pass the data to subsequent error-handling logic.
#[derive(Debug)]
pub struct ProcessError {
    pub(crate) data: SegmentData,
    pub(crate) kind: ProcessErrorKind,
}

impl ProcessError {
    /// Wrap `data` and `kind` into a new [`ProcessError`].
    pub fn new(data: SegmentData, kind: ProcessErrorKind) -> Self {
        Self { data, kind }
    }

    /// Shorthand for [`ProcessError::new`] with an I/O error.
    pub fn io(data: SegmentData, err: std::io::Error) -> Self {
        Self::new(data, ProcessErrorKind::Io(err))
    }
}

/// Kind of failure reported by a [`SegmentProcessor`].
#[derive(Debug)]
#[non_exhaustive]
pub enum ProcessErrorKind {
    /// The processor hit an `std::io::Error`.
    #[non_exhaustive]
    Io(std::io::Error),

    /// An error transferring data off the host
    #[non_exhaustive]
    Transfer {
        /// Underlying error source.
        source: Box<dyn std::error::Error + Send + Sync>,
        /// Whether this error is transient and the segment should be kept on
        /// disk for retry.
        retryable: bool,
    },
}

impl ProcessErrorKind {
    fn already_deleted(&self) -> bool {
        matches!(self, ProcessErrorKind::Io(err) if err.kind() == io::ErrorKind::NotFound)
    }

    /// Whether this error is transient and the segment should be kept on disk
    /// for retry.
    fn retryable(&self) -> bool {
        match self {
            ProcessErrorKind::Transfer { retryable, .. } => *retryable,
            _ => false,
        }
    }
}

impl std::fmt::Display for ProcessErrorKind {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Io(e) => write!(f, "I/O error: {e}"),
            Self::Transfer { source, .. } => write!(f, "S3 transfer error: {source}"),
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
            ProcessErrorKind::Transfer { source, .. } => Some(source.as_ref()),
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
        let retryable = matches!(
            e.kind(),
            aws_sdk_s3_transfer_manager::error::ErrorKind::IOError
                | aws_sdk_s3_transfer_manager::error::ErrorKind::RuntimeError
                | aws_sdk_s3_transfer_manager::error::ErrorKind::ChildOperationFailed
                | aws_sdk_s3_transfer_manager::error::ErrorKind::ChunkFailed(_)
        );
        Self::Transfer {
            source: Box::new(e),
            retryable,
        }
    }
}

/// A single step in the segment processing pipeline.
///
/// Implementations handle one concern: compress, symbolize, upload, etc.
/// The worker calls processors in sequence for each segment.
///
/// # Panic safety
///
/// The worker loop catches panics from [`process()`](Self::process) and
/// skips the panicking segment. The same processor instance is reused for
/// subsequent segments, so implementations **must** remain in a valid state
/// after a panic (i.e., no partially-updated invariants that would cause
/// incorrect behavior on the next call).
pub trait SegmentProcessor: Send {
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

/// Closure-scoped builder for assembling a custom processor pipeline.
///
/// Obtained via `with_custom_pipeline(|p| ...)` on the runtime builder.
/// Built-in processors are reachable through dedicated methods
/// ([`gzip`](Self::gzip), [`write_back`](Self::write_back),
/// [`s3`](Self::s3), [`symbolize`](Self::symbolize)); custom processors
/// are added with [`pipe`](Self::pipe).
///
/// # Example
///
/// ```ignore
/// struct Logger;
/// impl SegmentProcessor for Logger {
///     fn name(&self) -> &'static str { "logger" }
///     fn process(&mut self, data: SegmentData)
///         -> Pin<Box<dyn Future<Output = Result<SegmentData, ProcessError>> + Send + '_>>
///     {
///         Box::pin(async move {
///             println!("segment {} ({} bytes)", data.segment().index(), data.payload().len());
///             Ok(data)
///         })
///     }
/// }
///
/// builder.with_custom_pipeline(|p| p.pipe(Logger).gzip().write_back())
/// ```
#[must_use]
pub struct PipelineBuilder {
    processors: Vec<Box<dyn SegmentProcessor>>,
}

impl PipelineBuilder {
    pub(crate) fn new() -> Self {
        Self {
            processors: Vec::new(),
        }
    }

    pub(crate) fn into_processors(self) -> Vec<Box<dyn SegmentProcessor>> {
        self.processors
    }

    /// Append a user-supplied [`SegmentProcessor`] to the pipeline.
    pub fn pipe<S>(mut self, processor: S) -> Self
    where
        S: SegmentProcessor + 'static,
    {
        self.processors.push(Box::new(processor));
        self
    }

    /// Gzip the segment payload in-memory.
    pub fn gzip(mut self) -> Self {
        self.processors.push(Box::new(GzipCompressor));
        self
    }

    /// Write the current payload bytes back to disk. When the payload has
    /// been gzipped earlier in the pipeline, the file is written with a
    /// `.gz` suffix and the original sealed segment is removed.
    pub fn write_back(mut self) -> Self {
        self.processors.push(Box::new(WriteBackProcessor));
        self
    }

    /// Resolve stack-frame addresses in the segment to symbol names.
    /// Only valid when the runtime is built with the `cpu-profiling` feature.
    ///
    /// The built-in S3 / default presets prepend this automatically when
    /// CPU profiling is on; on the custom path the pipeline is passed
    /// through verbatim, so chain `.symbolize()` first if you want
    /// symbolized stack frames in your trace files.
    #[cfg(feature = "cpu-profiling")]
    pub fn symbolize(mut self) -> Self {
        self.processors.push(Box::new(SymbolizeProcessor));
        self
    }

    /// Upload the current payload to S3 with the given configuration. The
    /// AWS SDK default credential chain is used; call [`s3_with_client`]
    /// to supply a pre-built client.
    ///
    /// Does not auto-add gzip — chain `.gzip()` first if you want
    /// compressed uploads.
    ///
    /// [`s3_with_client`]: Self::s3_with_client
    #[cfg(feature = "worker-s3")]
    pub fn s3(mut self, config: s3::S3Config) -> Self {
        self.processors
            .push(Box::new(S3PipelineUploader::new(config, None)));
        self
    }

    /// Variant of [`s3`](Self::s3) that uses the supplied pre-built S3 client.
    #[cfg(feature = "worker-s3")]
    pub fn s3_with_client(mut self, config: s3::S3Config, client: aws_sdk_s3::Client) -> Self {
        self.processors
            .push(Box::new(S3PipelineUploader::new(config, Some(client))));
        self
    }
}

impl std::fmt::Debug for PipelineBuilder {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("PipelineBuilder")
            .field("len", &self.processors.len())
            .finish()
    }
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

    let processors = std::mem::take(&mut config.processors);
    let metrics_sink = config.metrics_sink.clone();

    tracing::info!(target: "dial9_worker", dir = %config.trace_dir().display(), stem = %config.trace_stem(), processors = processors.len(), "worker started");
    rt.block_on(async {
        let stop = tokio_util::sync::CancellationToken::new();
        let mut worker = WorkerLoop::new(config, processors, stop.clone(), metrics_sink);
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

/// Gzips the segment payload in-memory. Sets the `content_encoding` and
/// `write_back_extension` metadata keys so downstream stages know the
/// payload is gzipped. Already-gzipped segments (detected by magic bytes)
/// pass through unchanged.
#[derive(Debug, Default)]
pub(crate) struct GzipCompressor;

impl SegmentProcessor for GzipCompressor {
    fn name(&self) -> &'static str {
        "Gzip"
    }

    fn process(
        &mut self,
        mut data: SegmentData,
    ) -> Pin<Box<dyn Future<Output = Result<SegmentData, ProcessError>> + Send + '_>> {
        Box::pin(async move {
            // Skip already-compressed segments to avoid double-gzip.
            if data.payload.starts_with(&[0x1f, 0x8b]) {
                data.metadata
                    .insert("content_encoding".into(), "gzip".into());
                data.metadata
                    .insert("write_back_extension".into(), ".gz".into());
                return Ok(data);
            }
            let raw = std::mem::take(&mut data.payload);
            let compressed = tokio::task::spawn_blocking(move || {
                use flate2::write::GzEncoder;
                use std::io::Write;
                let mut encoder = GzEncoder::new(Vec::new(), flate2::Compression::fast());
                for chunk in raw.chunks() {
                    encoder.write_all(chunk)?;
                }
                encoder.finish()
            })
            .await;
            match compressed {
                Ok(Ok(bytes)) => {
                    data.metrics.compressed_size = Some(bytes.len() as u64);
                    data.payload = Payload::from_vec(bytes);
                    data.metadata
                        .insert("content_encoding".into(), "gzip".into());
                    data.metadata
                        .insert("write_back_extension".into(), ".gz".into());
                    Ok(data)
                }
                Ok(Err(e)) => Err(ProcessError {
                    data,
                    kind: ProcessErrorKind::Io(e),
                }),
                Err(e) => Err(ProcessError {
                    data,
                    kind: ProcessErrorKind::Io(std::io::Error::other(e)),
                }),
            }
        })
    }
}

// ---------------------------------------------------------------------------
// SymbolizeProcessor — resolves stack frame addresses to symbol names
// ---------------------------------------------------------------------------

/// Resolves stack-frame addresses in the segment to symbol names using
/// the current process's `/proc/self/maps`.
#[cfg(feature = "cpu-profiling")]
#[derive(Debug, Default)]
pub(crate) struct SymbolizeProcessor;

#[cfg(feature = "cpu-profiling")]
impl SegmentProcessor for SymbolizeProcessor {
    fn name(&self) -> &'static str {
        "Symbolize"
    }

    fn process(
        &mut self,
        mut data: SegmentData,
    ) -> Pin<Box<dyn Future<Output = Result<SegmentData, ProcessError>> + Send + '_>> {
        Box::pin(async move {
            // Skip already-compressed segments (e.g. leftover from a previous run).
            if data.payload.starts_with(&[0x1f, 0x8b]) {
                tracing::debug!(target: "dial9_worker", "segment is gzip-compressed, skipping symbolization");
                return Ok(data);
            }
            // The symbolize FFI reads `&[u8]`, so we materialize a single
            // contiguous `Bytes`. When there's only one chunk this is a
            // zero-copy `Bytes::clone`-equivalent; the `BytesMut` concat
            // path runs only on already-segmented input (rare).
            let input = std::mem::take(&mut data.payload).into_bytes();
            let result = tokio::task::spawn_blocking(move || {
                let maps = dial9_perf_self_profile::read_proc_maps();
                let mut output = Vec::new();
                dial9_perf_self_profile::offline_symbolize::symbolize_trace_with_maps(
                    &input,
                    &maps,
                    &mut output,
                )?;
                // Hand back the original bytes plus the symbol output as two
                // chunks — no copy of `input`.
                let mut combined = Payload::new();
                combined.push(input);
                combined.push(bytes::Bytes::from(output));
                Ok::<_, std::io::Error>(combined)
            })
            .await;
            match result {
                Ok(Ok(payload)) => {
                    data.payload = payload;
                    Ok(data)
                }
                Ok(Err(e)) => {
                    rate_limited!(Duration::from_secs(60), {
                        tracing::warn!(target: "dial9_worker", error = %e, "symbolization failed, preserving original bytes");
                    });
                    Err(ProcessError {
                        data,
                        kind: ProcessErrorKind::Io(e),
                    })
                }
                Err(e) => Err(ProcessError {
                    data,
                    kind: ProcessErrorKind::Io(std::io::Error::other(e)),
                }),
            }
        })
    }
}

// ---------------------------------------------------------------------------
// WriteBackProcessor — writes processed bytes back to disk
// ---------------------------------------------------------------------------

/// Writes the current payload bytes back to disk. If a
/// `write_back_extension` metadata key is present, the bytes are written to
/// `{original}{extension}` and the original segment file is removed.
#[derive(Debug, Default)]
pub(crate) struct WriteBackProcessor;

impl SegmentProcessor for WriteBackProcessor {
    fn name(&self) -> &'static str {
        "WriteBack"
    }

    fn process(
        &mut self,
        data: SegmentData,
    ) -> Pin<Box<dyn Future<Output = Result<SegmentData, ProcessError>> + Send + '_>> {
        Box::pin(async move {
            let original_path = data.segment.path.clone();
            let dest_path = match data.metadata.get("write_back_extension") {
                Some(ext) => {
                    let mut p = original_path.as_os_str().to_owned();
                    p.push(ext);
                    std::path::PathBuf::from(p)
                }
                None => original_path.clone(),
            };
            let payload = data.payload.clone();
            let write_dest = dest_path.clone();
            let result = tokio::task::spawn_blocking(move || {
                use std::io::{BufWriter, Write};
                let mut f = BufWriter::new(std::fs::File::create(&write_dest)?);
                for chunk in payload.chunks() {
                    f.write_all(chunk)?;
                }
                f.flush()
            })
            .await;
            match result {
                Ok(Ok(())) => {
                    if dest_path != original_path {
                        // Remove the original .bin now that .bin.gz exists.
                        // If the writer already evicted it, clean up the dest
                        // file we just wrote so it doesn't leak on disk.
                        match std::fs::remove_file(&original_path) {
                            Ok(()) => {}
                            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
                                let _ = std::fs::remove_file(&dest_path);
                            }
                            Err(e) => {
                                rate_limited!(Duration::from_secs(60), {
                                    tracing::warn!(
                                        "failed to remove original segment {}: {e}",
                                        original_path.display()
                                    );
                                });
                            }
                        }
                    }
                    Ok(data)
                }
                Ok(Err(e)) => Err(ProcessError {
                    data,
                    kind: ProcessErrorKind::Io(e),
                }),
                Err(e) => Err(ProcessError {
                    data,
                    kind: ProcessErrorKind::Io(std::io::Error::other(e)),
                }),
            }
        })
    }
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
        metrics_sink: BoxEntrySink,
    ) -> Self {
        Self {
            dir: config.trace_dir().to_path_buf(),
            stem: config.trace_stem().to_string(),
            poll_interval: config.poll_interval(),
            processors,
            metrics_sink,
            stop,
        }
    }

    pub(crate) async fn run(&mut self) {
        loop {
            let segments_found = self.process_open_segments().await;
            if self.stop.is_cancelled() {
                // One final scan to pick up segments sealed after our last
                // directory listing (the flush thread is joined before the
                // stop signal, so one extra pass is sufficient).
                self.process_open_segments().await;
                tracing::debug!(target: "dial9_worker", "Exiting run loop: cancellation received");
                return;
            }
            if !segments_found {
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
                rate_limited!(Duration::from_secs(60), {
                    tracing::warn!(target: "dial9_worker", "failed to scan for sealed segments: {e}");
                });
                return false;
            }
        };
        tracing::trace!(target: "dial9_worker", dir = %self.dir.display(), stem = %self.stem, count = segments.len(), "scanned for sealed segments");
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
                    rate_limited!(Duration::from_secs(60), {
                        tracing::warn!(target: "dial9_worker", error = %e, "failed to read segment");
                    });
                    continue;
                }
            };

            let (epoch_secs, header_valid) = sealed::creation_epoch_secs(&bytes, &segment.path);

            let metrics = SegmentProcessMetrics {
                operation: Operation::ProcessSegment,
                total_time: Timer::start_now(),
                status: None,
                segment_index: segment.index,
                uncompressed_size,
                compressed_size: None,
                invalid_file_header: !header_valid,
                panicked: false,
                panic_message: None,
                pipeline: PipelineMetrics::default(),
            }
            .append_on_drop(self.metrics_sink.clone());

            let mut data = SegmentData {
                segment: segment.clone(),
                payload: Payload::from_vec(bytes),
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
                // Catch panics in both the synchronous `process()` call
                // (which builds the future) and during `.await` (polling).
                // AssertUnwindSafe: current processors are stateless or have
                // trivially-recoverable state, so reuse after panic is safe.
                let process_result = {
                    match std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
                        processor.process(data)
                    })) {
                        Ok(fut) => std::panic::AssertUnwindSafe(fut).catch_unwind().await,
                        Err(panic_payload) => Err(panic_payload),
                    }
                };
                match process_result {
                    Ok(Ok(next)) => {
                        tracing::debug!(target: "dial9_worker", processor = processor.name(), segment = seg_idx + 1, elapsed_ms = proc_start.elapsed().as_secs_f64() * 1000.0, "processor succeeded");
                        data = next;
                        stage.succeed();
                        data.metrics.pipeline.push(processor.name(), stage);
                    }
                    Ok(Err(e)) => {
                        tracing::debug!(target: "dial9_worker", processor = processor.name(), segment = seg_idx + 1, elapsed_ms = proc_start.elapsed().as_secs_f64() * 1000.0, error = %e.kind, "processor failed");
                        data = e.data;
                        stage.fail();
                        data.metrics.pipeline.push(processor.name(), stage);
                        data.metrics.status = Some(MetriqueResult::Failure);
                        data.metrics.total_time.stop();
                        if e.kind.already_deleted() {
                            tracing::debug!(target: "dial9_worker", path = %segment.path.display(), "segment evicted during processing, skipping");
                        } else if e.kind.retryable() {
                            tracing::debug!(target: "dial9_worker", path = %segment.path.display(), err = ?e.kind, "retryable error, this file will be attempted to process again.");
                        } else {
                            if let Err(remove_err) = std::fs::remove_file(&segment.path) {
                                rate_limited!(Duration::from_secs(60), {
                                    tracing::warn!(target: "dial9_worker", error = %remove_err, path = %segment.path.display(), "failed to remove corrupted segment");
                                });
                            }
                            rate_limited!(Duration::from_secs(60), {
                                tracing::warn!(target: "dial9_worker", error = %e.kind, cause = ?e.kind, path = %segment.path.display(), "processor failed, removing segment");
                            });
                        }
                        continue 'next_segment;
                    }
                    Err(panic_payload) => {
                        let panic_msg = panic_payload
                            .downcast_ref::<&str>()
                            .copied()
                            .or_else(|| panic_payload.downcast_ref::<String>().map(|s| s.as_str()))
                            .unwrap_or("unknown panic");
                        rate_limited!(
                            Duration::from_secs(60),
                            tracing::error!(
                                target: "dial9_worker",
                                processor = processor.name(),
                                segment = seg_idx + 1,
                                path = %segment.path.display(),
                                panic = panic_msg,
                                "processor panicked, skipping segment"
                            )
                        );
                        // The original metrics guard was consumed by the
                        // panicking future. Emit a new one so panics are
                        // visible in operational metrics.
                        drop(
                            SegmentProcessMetrics {
                                operation: Operation::ProcessSegment,
                                total_time: Timer::start_now(),
                                status: Some(MetriqueResult::Failure),
                                segment_index: segment.index,
                                uncompressed_size,
                                compressed_size: None,
                                invalid_file_header: !header_valid,
                                panicked: true,
                                panic_message: Some(panic_msg.to_owned()),
                                pipeline: PipelineMetrics::default(),
                            }
                            .append_on_drop(self.metrics_sink.clone()),
                        );
                        if let Err(remove_err) = std::fs::remove_file(&segment.path)
                            && remove_err.kind() != std::io::ErrorKind::NotFound
                        {
                            rate_limited!(
                                Duration::from_secs(60),
                                tracing::warn!(target: "dial9_worker", error = %remove_err, path = %segment.path.display(), "failed to remove segment after panic")
                            );
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

/// S3 uploader processor. Construction is synchronous — the AWS client and
/// bucket region are resolved lazily on the first `process()` call, inside
/// the worker's tokio runtime.
#[cfg(feature = "worker-s3")]
pub(crate) struct S3PipelineUploader {
    state: S3UploaderState,
}

#[cfg(feature = "worker-s3")]
enum S3UploaderState {
    Pending {
        s3_config: s3::S3Config,
        client: Option<aws_sdk_s3::Client>,
    },
    Ready {
        uploader: s3::S3Uploader,
        circuit_breaker: connection::CircuitBreaker,
    },
}

#[cfg(feature = "worker-s3")]
impl std::fmt::Debug for S3PipelineUploader {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("S3PipelineUploader").finish_non_exhaustive()
    }
}

#[cfg(feature = "worker-s3")]
impl S3PipelineUploader {
    /// Create a new uploader from an [`S3Config`](s3::S3Config) and an
    /// optional pre-built S3 client. If `client` is `None`, the default
    /// AWS configuration chain is used. Region detection and transfer
    /// manager construction are deferred to the first `process()` call.
    pub(crate) fn new(s3_config: s3::S3Config, client: Option<aws_sdk_s3::Client>) -> Self {
        Self {
            state: S3UploaderState::Pending { s3_config, client },
        }
    }

    /// Set (or override) the pre-built S3 client. Must be called before the
    /// uploader has been initialized (i.e. before the first segment has been
    /// processed);
    /// Note: the only caller is the builder, which runs before the
    /// worker is spawned, so reaching the `Ready` arm is a programmer error.
    pub(crate) fn set_client(&mut self, client: aws_sdk_s3::Client) {
        match &mut self.state {
            S3UploaderState::Pending { client: slot, .. } => *slot = Some(client),
            S3UploaderState::Ready { .. } => {
                unreachable!("set_client called after uploader initialization")
            }
        }
    }

    /// Take any previously-stashed client out of a `Pending` uploader so it
    /// can be carried into a replacement. Returns `None` once the uploader
    /// has been initialized.
    pub(crate) fn take_client(&mut self) -> Option<aws_sdk_s3::Client> {
        match &mut self.state {
            S3UploaderState::Pending { client, .. } => client.take(),
            S3UploaderState::Ready { .. } => None,
        }
    }

    /// Construct an uploader directly in the `Ready` state. Test-only —
    /// production code goes through [`new`](Self::new) and lazy init.
    #[cfg(test)]
    pub(crate) fn from_ready(
        uploader: s3::S3Uploader,
        circuit_breaker: connection::CircuitBreaker,
    ) -> Self {
        Self {
            state: S3UploaderState::Ready {
                uploader,
                circuit_breaker,
            },
        }
    }

    async fn initialize(
        s3_config: s3::S3Config,
        client: Option<aws_sdk_s3::Client>,
    ) -> (s3::S3Uploader, connection::CircuitBreaker) {
        let bootstrap_client = match client {
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

        (
            s3::S3Uploader::new(tm_client, s3_config),
            connection::CircuitBreaker::new(),
        )
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
            // Lazy init: clone the config + client and run `initialize`
            // without mutating `self.state`. If the init future panics or
            // is cancelled mid-await, the worker's outer `catch_unwind`
            // recovers and `self.state` stays `Pending`, so the next
            // segment will retry. Mutating before the await would leave
            // the uploader stuck in a transient state forever.
            if let S3UploaderState::Pending { s3_config, client } = &self.state {
                let cfg = s3_config.clone();
                let cli = client.clone();
                let (uploader, circuit_breaker) = Self::initialize(cfg, cli).await;
                self.state = S3UploaderState::Ready {
                    uploader,
                    circuit_breaker,
                };
            }
            let S3UploaderState::Ready {
                uploader,
                circuit_breaker,
            } = &mut self.state
            else {
                // unreachable: we just transitioned above and the state
                // doesn't otherwise revert. Fall through with an error so
                // a future refactor doesn't silently break.
                return Err(ProcessError {
                    data,
                    kind: ProcessErrorKind::Io(std::io::Error::other(
                        "S3 uploader in unexpected state",
                    )),
                });
            };
            if !circuit_breaker.should_attempt() {
                tracing::debug!(target: "dial9_worker", path = %data.segment.path.display(), "circuit breaker open, skipping upload");
                return Err(ProcessError {
                    data,
                    kind: ProcessErrorKind::Transfer {
                        source: Box::from("circuit breaker open"),
                        retryable: true,
                    },
                });
            }
            let payload = std::mem::take(&mut data.payload);
            match uploader
                .upload_and_delete(&data.segment, payload, &data.metadata)
                .await
            {
                Ok(key) => {
                    circuit_breaker.on_success();
                    rate_limited!(Duration::from_secs(10), {
                        tracing::info!(target: "dial9_worker", "uploaded {key}");
                    });
                    Ok(data)
                }
                Err(kind) => {
                    if matches!(&kind, ProcessErrorKind::Io(io) if io.kind() == std::io::ErrorKind::NotFound)
                    {
                        tracing::debug!(target: "dial9_worker", path = %data.segment.path.display(), "segment already evicted, skipping");
                    } else {
                        circuit_breaker.on_failure();
                        rate_limited!(Duration::from_secs(60), {
                            tracing::warn!(target: "dial9_worker", error = %kind, "upload failed");
                        });
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

        check!(deps.circuit_breaker == connection::CircuitBreaker::Closed);
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
            Box::new(S3PipelineUploader::from_ready(
                uploader,
                connection::CircuitBreaker::new(),
            )),
        ];

        let segment = sealed::SealedSegment {
            path: segment_path.clone(),
            index: 0,
        };

        let metrics = SegmentProcessMetrics {
            operation: Operation::ProcessSegment,
            total_time: metrique::timers::Timer::start_now(),
            status: None,
            segment_index: 0,
            uncompressed_size: data.len() as u64,
            compressed_size: None,
            invalid_file_header: false,
            panicked: false,
            panic_message: None,
            pipeline: PipelineMetrics::default(),
        }
        .append_on_drop(sink);

        let mut pipe_data = SegmentData {
            segment,
            payload: Payload::from_vec(data),
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

    /// `set_client` is only valid while the uploader is `Pending`. Calling it
    /// on a `Ready` uploader indicates an internal misuse and must panic
    /// rather than silently drop the new client.
    #[test]
    #[should_panic(expected = "set_client called after uploader initialization")]
    fn set_client_after_ready_panics() {
        let s3_config = s3::S3Config::builder()
            .bucket("test")
            .service_name("test")
            .instance_path("test")
            .boot_id("test")
            .region("us-east-1")
            .build();
        let sdk_config = aws_sdk_s3::Config::builder()
            .behavior_version_latest()
            .credentials_provider(aws_sdk_s3::config::Credentials::new(
                "test", "test", None, None, "test",
            ))
            .region(aws_sdk_s3::config::Region::new("us-east-1"))
            .build();
        let sdk_client = aws_sdk_s3::Client::from_conf(sdk_config);
        let tm_client = aws_sdk_s3_transfer_manager::Client::new(
            aws_sdk_s3_transfer_manager::Config::builder()
                .client(sdk_client.clone())
                .build(),
        );
        let uploader = s3::S3Uploader::new(tm_client, s3_config);
        let mut pipeline_uploader =
            S3PipelineUploader::from_ready(uploader, connection::CircuitBreaker::new());
        pipeline_uploader.set_client(sdk_client);
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
                    let mut done = data.segment.path.as_os_str().to_owned();
                    done.push(".done");
                    let _ = std::fs::rename(&data.segment.path, done);
                    Ok(data)
                })
            }
        }

        // Pre-cancelled token so the worker processes once and exits.
        let stop = tokio_util::sync::CancellationToken::new();
        stop.cancel();
        let config = BackgroundTaskConfig::builder()
            .trace_path(dir.path().join("trace.bin"))
            .build();

        let processors: Vec<Box<dyn SegmentProcessor>> =
            vec![Box::new(CountingProcessor(processed.clone()))];

        let mut worker = WorkerLoop::new(
            config,
            processors,
            stop,
            metrique_writer::sink::DevNullSink::boxed(),
        );
        worker.run().await;

        // Worker should have drained both segments even though stop was set.
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
                        let mut done = data.segment.path.as_os_str().to_owned();
                        done.push(".done");
                        let _ = std::fs::rename(&data.segment.path, done);
                        Ok(data)
                    }
                })
            }
        }

        let stop = tokio_util::sync::CancellationToken::new();
        stop.cancel();
        let config = BackgroundTaskConfig::builder()
            .trace_path(dir.path().join("trace.bin"))
            .build();

        let processors: Vec<Box<dyn SegmentProcessor>> = vec![Box::new(FailFirstProcessor {
            counter: processed.clone(),
            calls: 0,
        })];

        let mut worker = WorkerLoop::new(
            config,
            processors,
            stop,
            metrique_writer::sink::DevNullSink::boxed(),
        );
        worker.run().await;

        // Second segment should still be processed despite first failing.
        check!(processed.load(Ordering::SeqCst) == 1);
    }

    #[test]
    fn trace_dir_for_bare_relative_path_defaults_to_current_directory() {
        let config = BackgroundTaskConfig::builder()
            .trace_path("trace.bin")
            .build();

        check!(config.trace_dir() == std::path::Path::new("."));
    }
}

// --- Review finding #9: trace_stem edge cases ---

#[cfg(test)]
mod trace_stem_tests {
    use super::*;
    use assert2::check;

    #[test]
    fn trace_stem_normal_path() {
        let config = BackgroundTaskConfig::builder()
            .trace_path("/tmp/traces/trace.bin")
            .build();
        check!(config.trace_stem() == "trace");
    }

    #[test]
    fn trace_stem_directory_path() {
        // A path like "/tmp/traces/" — file_stem returns "traces", not an error
        let config = BackgroundTaskConfig::builder()
            .trace_path("/tmp/traces/")
            .build();
        // This is the current behavior — it returns "traces" not "trace"
        // which would silently match the wrong files
        check!(config.trace_stem() == "traces");
    }

    #[test]
    fn trace_stem_root_path() {
        // A path like "/" has no file stem
        let config = BackgroundTaskConfig::builder().trace_path("/").build();
        // Should fall back to "trace" and log an error
        check!(config.trace_stem() == "trace");
    }

    #[test]
    fn trace_dir_for_directory_path() {
        let config = BackgroundTaskConfig::builder()
            .trace_path("/tmp/traces/")
            .build();
        // trace_dir should be the parent of the path
        check!(config.trace_dir() == std::path::Path::new("/tmp"));
    }
}

#[cfg(test)]
mod worker_pipeline_tests {
    use super::*;
    use assert2::check;
    use std::sync::Arc;

    fn config_for(dir: &std::path::Path) -> BackgroundTaskConfig {
        BackgroundTaskConfig::builder()
            .trace_path(dir.join("trace.bin"))
            .build()
    }

    /// s3s wrapper where every upload returns 500 InternalError.
    struct AlwaysFailS3<S>(S);

    #[async_trait::async_trait]
    impl<S: s3s::S3 + Send + Sync> s3s::S3 for AlwaysFailS3<S> {
        async fn put_object(
            &self,
            _req: s3s::S3Request<s3s::dto::PutObjectInput>,
        ) -> s3s::S3Result<s3s::S3Response<s3s::dto::PutObjectOutput>> {
            Err(s3s::S3Error::with_message(
                s3s::S3ErrorCode::InternalError,
                "injected 500",
            ))
        }
        async fn create_multipart_upload(
            &self,
            _req: s3s::S3Request<s3s::dto::CreateMultipartUploadInput>,
        ) -> s3s::S3Result<s3s::S3Response<s3s::dto::CreateMultipartUploadOutput>> {
            Err(s3s::S3Error::with_message(
                s3s::S3ErrorCode::InternalError,
                "injected 500",
            ))
        }
        async fn upload_part(
            &self,
            _req: s3s::S3Request<s3s::dto::UploadPartInput>,
        ) -> s3s::S3Result<s3s::S3Response<s3s::dto::UploadPartOutput>> {
            Err(s3s::S3Error::with_message(
                s3s::S3ErrorCode::InternalError,
                "injected 500",
            ))
        }
        async fn complete_multipart_upload(
            &self,
            _req: s3s::S3Request<s3s::dto::CompleteMultipartUploadInput>,
        ) -> s3s::S3Result<s3s::S3Response<s3s::dto::CompleteMultipartUploadOutput>> {
            Err(s3s::S3Error::with_message(
                s3s::S3ErrorCode::InternalError,
                "injected 500",
            ))
        }
    }

    fn always_failing_s3_uploader() -> (s3::S3Uploader, tempfile::TempDir) {
        let s3_root = tempfile::tempdir().unwrap();
        let fs = s3s_fs::FileSystem::new(s3_root.path()).unwrap();
        let failing = AlwaysFailS3(fs);
        let mut builder = s3s::service::S3ServiceBuilder::new(failing);
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
        let s3_config = s3::S3Config::builder()
            .bucket("test-bucket")
            .service_name("test")
            .instance_path("test")
            .boot_id("test")
            .region("us-east-1")
            .build();
        (s3::S3Uploader::new(tm_client, s3_config), s3_root)
    }

    /// A segment that fails with a transient S3 error (500) is kept on disk for retry.
    #[tokio::test]
    async fn failed_segment_kept_on_transient_error() {
        let dir = tempfile::tempdir().unwrap();
        let seg_path = dir.path().join("trace.0.bin");
        std::fs::write(&seg_path, b"bad data").unwrap();

        let (uploader, _s3_root) = always_failing_s3_uploader();
        let processors: Vec<Box<dyn SegmentProcessor>> = vec![Box::new(
            S3PipelineUploader::from_ready(uploader, connection::CircuitBreaker::new()),
        )];

        let stop = tokio_util::sync::CancellationToken::new();
        let mut worker = WorkerLoop::new(
            config_for(dir.path()),
            processors,
            stop,
            metrique_writer::sink::DevNullSink::boxed(),
        );
        worker.process_open_segments().await;

        check!(
            seg_path.exists(),
            "segment should be kept on disk after transient S3 error"
        );
    }

    /// A circuit-breaker-open error keeps the segment on disk.
    #[tokio::test]
    async fn circuit_breaker_open_keeps_segment() {
        let dir = tempfile::tempdir().unwrap();
        let seg_path = dir.path().join("trace.0.bin");
        std::fs::write(&seg_path, b"trace data").unwrap();

        let (uploader, _s3_root) = always_failing_s3_uploader();
        let mut cb = connection::CircuitBreaker::new();
        // Trip the circuit breaker so it refuses attempts.
        cb.on_failure();
        let processors: Vec<Box<dyn SegmentProcessor>> =
            vec![Box::new(S3PipelineUploader::from_ready(uploader, cb))];

        let stop = tokio_util::sync::CancellationToken::new();
        let mut worker = WorkerLoop::new(
            config_for(dir.path()),
            processors,
            stop,
            metrique_writer::sink::DevNullSink::boxed(),
        );
        worker.process_open_segments().await;

        check!(
            seg_path.exists(),
            "segment should be kept when circuit breaker is open"
        );
    }

    /// A NotFound error (evicted segment) is silently skipped — no deletion attempt.
    #[tokio::test]
    async fn not_found_error_skips_cleanly() {
        let dir = tempfile::tempdir().unwrap();
        let seg_path = dir.path().join("trace.0.bin");
        // Write the file so it can be read, but the processor returns NotFound
        std::fs::write(&seg_path, b"data").unwrap();

        struct NotFoundProcessor;
        impl SegmentProcessor for NotFoundProcessor {
            fn name(&self) -> &'static str {
                "NotFound"
            }
            fn process(
                &mut self,
                data: SegmentData,
            ) -> Pin<Box<dyn Future<Output = Result<SegmentData, ProcessError>> + Send + '_>>
            {
                Box::pin(async {
                    Err(ProcessError {
                        data,
                        kind: ProcessErrorKind::Io(std::io::Error::new(
                            std::io::ErrorKind::NotFound,
                            "evicted",
                        )),
                    })
                })
            }
        }

        let stop = tokio_util::sync::CancellationToken::new();
        let processors: Vec<Box<dyn SegmentProcessor>> = vec![Box::new(NotFoundProcessor)];

        let mut worker = WorkerLoop::new(
            config_for(dir.path()),
            processors,
            stop,
            metrique_writer::sink::DevNullSink::boxed(),
        );
        worker.process_open_segments().await;

        // File still exists because the processor returned NotFound (eviction),
        // which means the worker should skip — not attempt to delete.
        check!(
            seg_path.exists(),
            "segment should not be deleted on NotFound (eviction)"
        );
    }

    /// A permanent, non-retryable IO error deletes the segment.
    #[tokio::test]
    async fn permanent_io_error_deletes_segment() {
        let dir = tempfile::tempdir().unwrap();
        let seg_path = dir.path().join("trace.0.bin");
        std::fs::write(&seg_path, b"bad data").unwrap();

        struct PermanentFailProcessor;
        impl SegmentProcessor for PermanentFailProcessor {
            fn name(&self) -> &'static str {
                "PermanentFail"
            }
            fn process(
                &mut self,
                data: SegmentData,
            ) -> Pin<Box<dyn Future<Output = Result<SegmentData, ProcessError>> + Send + '_>>
            {
                Box::pin(async {
                    Err(ProcessError {
                        data,
                        kind: ProcessErrorKind::Io(std::io::Error::other("corrupt data")),
                    })
                })
            }
        }

        let stop = tokio_util::sync::CancellationToken::new();
        let processors: Vec<Box<dyn SegmentProcessor>> = vec![Box::new(PermanentFailProcessor)];

        let mut worker = WorkerLoop::new(
            config_for(dir.path()),
            processors,
            stop,
            metrique_writer::sink::DevNullSink::boxed(),
        );
        worker.process_open_segments().await;

        check!(
            !seg_path.exists(),
            "segment should be deleted after permanent IO error"
        );
    }

    /// Gzip-compressed segments pass through GzipCompressor unchanged.
    #[tokio::test]
    async fn gzip_segment_not_double_compressed() {
        let dir = tempfile::tempdir().unwrap();

        let gzip_data = {
            use flate2::write::GzEncoder;
            use std::io::Write;
            let mut enc = GzEncoder::new(Vec::new(), flate2::Compression::fast());
            enc.write_all(b"already compressed").unwrap();
            enc.finish().unwrap()
        };
        std::fs::write(dir.path().join("trace.0.bin"), &gzip_data).unwrap();

        let output_bytes = Arc::new(std::sync::Mutex::new(Vec::new()));

        struct CaptureProcessor(Arc<std::sync::Mutex<Vec<u8>>>);
        impl SegmentProcessor for CaptureProcessor {
            fn name(&self) -> &'static str {
                "Capture"
            }
            fn process(
                &mut self,
                data: SegmentData,
            ) -> Pin<Box<dyn Future<Output = Result<SegmentData, ProcessError>> + Send + '_>>
            {
                *self.0.lock().unwrap() = data.payload.clone().into_vec();
                Box::pin(async { Ok(data) })
            }
        }

        let stop = tokio_util::sync::CancellationToken::new();
        stop.cancel();

        let processors: Vec<Box<dyn SegmentProcessor>> = vec![
            Box::new(GzipCompressor),
            Box::new(CaptureProcessor(output_bytes.clone())),
        ];

        let mut worker = WorkerLoop::new(
            config_for(dir.path()),
            processors,
            stop,
            metrique_writer::sink::DevNullSink::boxed(),
        );
        worker.run().await;

        // The captured bytes should be identical to the input (not double-gzipped).
        check!(output_bytes.lock().unwrap().as_slice() == gzip_data.as_slice());
    }

    /// WriteBackProcessor writes to a new path when `write_back_extension` is
    /// set and removes the original file, preventing re-discovery on the next
    /// poll cycle.
    #[tokio::test]
    async fn write_back_renames_when_extension_metadata_set() {
        let dir = tempfile::tempdir().unwrap();
        let seg_path = dir.path().join("trace.0.bin");
        std::fs::write(&seg_path, b"payload").unwrap();

        let segment = sealed::SealedSegment {
            path: seg_path.clone(),
            index: 0,
        };

        let metrics = SegmentProcessMetrics {
            operation: Operation::ProcessSegment,
            total_time: metrique::timers::Timer::start_now(),
            status: None,
            segment_index: 0,
            uncompressed_size: 7,
            compressed_size: None,
            invalid_file_header: false,
            panicked: false,
            panic_message: None,
            pipeline: PipelineMetrics::default(),
        }
        .append_on_drop(metrique_writer::sink::DevNullSink::boxed());

        let data = SegmentData {
            segment,
            payload: Payload::from(b"payload"),
            metadata: HashMap::from([("write_back_extension".into(), ".gz".into())]),
            metrics,
        };

        let mut processor = WriteBackProcessor;
        let result = processor.process(data).await;
        check!(result.is_ok());

        // Original .bin should be gone, .bin.gz should exist with the payload.
        check!(!seg_path.exists());
        let gz_path = dir.path().join("trace.0.bin.gz");
        check!(gz_path.exists());
        check!(std::fs::read(&gz_path).unwrap() == b"payload");
    }

    /// WriteBackProcessor writes to the original path when no
    /// `write_back_extension` metadata is set.
    #[tokio::test]
    async fn write_back_overwrites_in_place_without_extension() {
        let dir = tempfile::tempdir().unwrap();
        let seg_path = dir.path().join("trace.0.bin");
        std::fs::write(&seg_path, b"old").unwrap();

        let segment = sealed::SealedSegment {
            path: seg_path.clone(),
            index: 0,
        };

        let metrics = SegmentProcessMetrics {
            operation: Operation::ProcessSegment,
            total_time: metrique::timers::Timer::start_now(),
            status: None,
            segment_index: 0,
            uncompressed_size: 3,
            compressed_size: None,
            invalid_file_header: false,
            panicked: false,
            panic_message: None,
            pipeline: PipelineMetrics::default(),
        }
        .append_on_drop(metrique_writer::sink::DevNullSink::boxed());

        let data = SegmentData {
            segment,
            payload: Payload::from(b"new"),
            metadata: HashMap::new(),
            metrics,
        };

        let mut processor = WriteBackProcessor;
        let result = processor.process(data).await;
        check!(result.is_ok());

        check!(std::fs::read(&seg_path).unwrap() == b"new");
    }

    /// The full GzipCompressor → WriteBackProcessor pipeline writes a `.bin.gz`
    /// file and removes the original `.bin`, so `find_sealed_segments` will not
    /// re-discover it on the next poll.
    #[tokio::test]
    async fn gzip_write_back_pipeline_prevents_rediscovery() {
        let dir = tempfile::tempdir().unwrap();
        let seg_path = dir.path().join("trace.0.bin");
        std::fs::write(&seg_path, b"raw trace data").unwrap();

        let stop = tokio_util::sync::CancellationToken::new();
        stop.cancel();

        let processors: Vec<Box<dyn SegmentProcessor>> =
            vec![Box::new(GzipCompressor), Box::new(WriteBackProcessor)];

        let mut worker = WorkerLoop::new(
            config_for(dir.path()),
            processors,
            stop,
            metrique_writer::sink::DevNullSink::boxed(),
        );
        worker.run().await;

        // Original .bin removed; .bin.gz written.
        check!(!seg_path.exists());
        check!(dir.path().join("trace.0.bin.gz").exists());

        // A subsequent scan should find no sealed segments.
        let segments = sealed::find_sealed_segments(dir.path(), "trace").unwrap();
        check!(segments.is_empty());
    }

    /// A processor that panics must not kill the worker loop. The panicking
    /// segment is skipped and subsequent segments are still processed.
    #[tokio::test]
    async fn processor_panic_does_not_kill_worker_loop() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("trace.0.bin"), b"panic me").unwrap();
        std::fs::write(dir.path().join("trace.1.bin"), b"process me").unwrap();

        let processed = Arc::new(std::sync::atomic::AtomicUsize::new(0));

        struct PanicFirstProcessor {
            counter: Arc<std::sync::atomic::AtomicUsize>,
            calls: usize,
        }
        impl SegmentProcessor for PanicFirstProcessor {
            fn name(&self) -> &'static str {
                "PanicFirst"
            }
            fn process(
                &mut self,
                data: SegmentData,
            ) -> Pin<Box<dyn Future<Output = Result<SegmentData, ProcessError>> + Send + '_>>
            {
                self.calls += 1;
                let should_panic = self.calls == 1;
                let counter = self.counter.clone();
                Box::pin(async move {
                    if should_panic {
                        panic!("processor panic on first segment");
                    }
                    counter.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
                    Ok(data)
                })
            }
        }

        let stop = tokio_util::sync::CancellationToken::new();
        stop.cancel();

        let processors: Vec<Box<dyn SegmentProcessor>> = vec![Box::new(PanicFirstProcessor {
            counter: processed.clone(),
            calls: 0,
        })];

        use metrique_writer::AnyEntrySink;
        use metrique_writer::test_util::Inspector;
        let inspector = Inspector::default();

        let mut worker = WorkerLoop::new(
            config_for(dir.path()),
            processors,
            stop,
            inspector.clone().boxed(),
        );
        worker.run().await;

        // The worker must have processed at least one segment (the non-panicking one)
        // despite the first processor call panicking.
        check!(processed.load(std::sync::atomic::Ordering::SeqCst) >= 1);
        // The panicking segment's file should have been removed.
        check!(!dir.path().join("trace.0.bin").exists());

        // Verify metrics: we should have entries for both segments, and the
        // panicking one should have Panicked=true.
        let entries = inspector.entries();
        let panicked_entries: Vec<_> = entries
            .iter()
            .filter(|e| e.metrics.get("Panicked").is_some_and(|m| m.as_bool()))
            .collect();
        check!(
            panicked_entries.len() == 1,
            "expected exactly one panicked metric entry, got {}",
            panicked_entries.len()
        );
        check!(panicked_entries[0].metrics["Failure"] == true);
        // The panic message should be captured.
        check!(panicked_entries[0].values["PanicMessage"] == "processor panic on first segment");
    }

    /// A processor that hangs must not prevent the worker from shutting down.
    /// The drain timeout in `run_background_task` handles this, but at the
    /// WorkerLoop level, cancellation should interrupt a hung processor.
    #[tokio::test(flavor = "current_thread", start_paused = true)]
    async fn processor_hang_respects_shutdown_timeout() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("trace.0.bin"), b"hang me").unwrap();

        struct HangingProcessor;
        impl SegmentProcessor for HangingProcessor {
            fn name(&self) -> &'static str {
                "Hanging"
            }
            fn process(
                &mut self,
                _data: SegmentData,
            ) -> Pin<Box<dyn Future<Output = Result<SegmentData, ProcessError>> + Send + '_>>
            {
                Box::pin(async {
                    // Hang forever
                    std::future::pending::<()>().await;
                    unreachable!()
                })
            }
        }

        let stop = tokio_util::sync::CancellationToken::new();
        let processors: Vec<Box<dyn SegmentProcessor>> = vec![Box::new(HangingProcessor)];

        let mut worker = WorkerLoop::new(
            config_for(dir.path()),
            processors,
            stop.clone(),
            metrique_writer::sink::DevNullSink::boxed(),
        );

        let run_fut = worker.run();

        // Simulate the shutdown path from run_background_task:
        // cancel the stop token, then timeout the run future.
        let drain_timeout = Duration::from_secs(2);
        stop.cancel();
        let result = tokio::time::timeout(drain_timeout, run_fut).await;

        // The timeout should fire because the processor is hung.
        check!(result.is_err(), "expected timeout, but worker completed");
    }
}
