pub mod sealed;
#[cfg(feature = "worker")]
pub mod identity;
#[cfg(feature = "worker-s3")]
pub mod connection;
#[cfg(feature = "worker-s3")]
pub mod s3;

#[cfg(feature = "worker")]
mod worker_config {
    use std::path::{Path, PathBuf};
    use std::time::Duration;

    const DEFAULT_POLL_INTERVAL: Duration = Duration::from_secs(1);

    /// Configuration for the in-process worker pipeline.
    pub struct WorkerConfig {
        poll_interval: Duration,
        trace_path: PathBuf,
        #[cfg(feature = "worker-s3")]
        s3: Option<super::s3::S3Config>,
    }

    impl WorkerConfig {
        pub fn builder() -> WorkerConfigBuilder {
            WorkerConfigBuilder::default()
        }

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

        /// S3 upload configuration, if any.
        #[cfg(feature = "worker-s3")]
        pub fn s3(&self) -> Option<&super::s3::S3Config> {
            self.s3.as_ref()
        }
    }

    #[derive(Default)]
    pub struct WorkerConfigBuilder {
        poll_interval: Option<Duration>,
        trace_path: Option<PathBuf>,
        #[cfg(feature = "worker-s3")]
        s3: Option<super::s3::S3Config>,
    }

    impl WorkerConfigBuilder {
        /// Set the poll interval for checking sealed segments.
        /// Defaults to 1 second.
        pub fn poll_interval(mut self, interval: Duration) -> Self {
            self.poll_interval = Some(interval);
            self
        }

        /// Set the trace base path (same path passed to `RotatingWriter::new`).
        pub fn trace_path(mut self, path: impl Into<PathBuf>) -> Self {
            self.trace_path = Some(path.into());
            self
        }

        /// Set S3 upload configuration.
        #[cfg(feature = "worker-s3")]
        pub fn s3(mut self, config: super::s3::S3Config) -> Self {
            self.s3 = Some(config);
            self
        }

        pub fn build(self) -> Result<WorkerConfig, &'static str> {
            Ok(WorkerConfig {
                poll_interval: self.poll_interval.unwrap_or(DEFAULT_POLL_INTERVAL),
                trace_path: self.trace_path.ok_or("trace_path is required")?,
                #[cfg(feature = "worker-s3")]
                s3: self.s3,
            })
        }
    }
}

#[cfg(feature = "worker")]
pub use worker_config::*;

/// The worker loop function. Runs on a dedicated thread, polls for sealed
/// segments and processes them (upload to S3 if configured).
#[cfg(feature = "worker")]
pub(crate) fn run_worker(
    config: WorkerConfig,
    stop: std::sync::Arc<std::sync::atomic::AtomicBool>,
) {
    let dir = config.trace_dir().to_path_buf();
    let stem = config.trace_stem().to_string();
    let poll_interval = config.poll_interval();

    #[cfg(feature = "worker-s3")]
    let mut s3_state = s3_worker_state(&config);

    loop {
        if stop.load(std::sync::atomic::Ordering::Acquire) {
            // Drain remaining sealed segments before exiting
            if let Ok(segments) = sealed::find_sealed_segments(&dir, &stem) {
                for segment in &segments {
                    #[cfg(feature = "worker-s3")]
                    process_segment(segment, &mut s3_state);
                    let _ = segment; // suppress unused warning when worker-s3 is off
                }
            }
            return;
        }

        match sealed::find_sealed_segments(&dir, &stem) {
            Ok(segments) if segments.is_empty() => {}
            Ok(segments) => {
                for segment in &segments {
                    if stop.load(std::sync::atomic::Ordering::Acquire) {
                        // Still process this segment, then exit
                        #[cfg(feature = "worker-s3")]
                        process_segment(segment, &mut s3_state);
                        let _ = segment;
                        // Drain remaining
                        continue;
                    }
                    #[cfg(feature = "worker-s3")]
                    process_segment(segment, &mut s3_state);
                    let _ = segment;
                }
            }
            Err(e) => {
                tracing::warn!(target: "dial9_worker", "failed to scan for sealed segments: {e}");
            }
        }

        std::thread::sleep(poll_interval);
    }
}

#[cfg(feature = "worker-s3")]
struct S3WorkerState {
    uploader: s3::S3Uploader,
    connection: connection::S3ConnectionState,
    runtime: tokio::runtime::Runtime,
}

#[cfg(feature = "worker-s3")]
fn s3_worker_state(config: &WorkerConfig) -> Option<S3WorkerState> {
    let s3_config = config.s3()?.clone();
    let rt = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .ok()?;
    let sdk_config = rt.block_on(aws_config::load_defaults(aws_config::BehaviorVersion::latest()));
    let s3_client = aws_sdk_s3_transfer_manager::Client::new(
        aws_sdk_s3_transfer_manager::Config::builder()
            .client(aws_sdk_s3::Client::new(&sdk_config))
            .build(),
    );
    Some(S3WorkerState {
        uploader: s3::S3Uploader::new(s3_client, s3_config),
        connection: connection::S3ConnectionState::healthy(),
        runtime: rt,
    })
}

#[cfg(feature = "worker-s3")]
fn process_segment(segment: &sealed::SealedSegment, state: &mut Option<S3WorkerState>) {
    let Some(state) = state.as_mut() else { return };
    if !state.connection.should_attempt_upload() {
        return;
    }
    let timestamp = chrono_free_timestamp();
    match state.runtime.block_on(state.uploader.upload_and_delete(segment, &timestamp)) {
        Ok(key) => {
            state.connection.on_success();
            tracing::info!(target: "dial9_worker", "uploaded {key}");
        }
        Err(e) => {
            state.connection.on_failure();
            tracing::warn!(target: "dial9_worker", "upload failed: {e}");
        }
    }
}

/// Generate a timestamp string without chrono dependency.
/// Format: YYYY-MM-DDTHH-MM-SSZ (colons replaced with dashes per design doc §5).
#[cfg(feature = "worker-s3")]
fn chrono_free_timestamp() -> String {
    use std::time::SystemTime;
    let now = SystemTime::now()
        .duration_since(SystemTime::UNIX_EPOCH)
        .unwrap_or_default();
    let secs = now.as_secs();
    let days = secs / 86400;
    let time_of_day = secs % 86400;
    let hours = time_of_day / 3600;
    let minutes = (time_of_day % 3600) / 60;
    let seconds = time_of_day % 60;

    let (year, month, day) = days_to_ymd(days);
    format!(
        "{:04}-{:02}-{:02}T{:02}-{:02}-{:02}Z",
        year, month, day, hours, minutes, seconds
    )
}

#[cfg(feature = "worker-s3")]
fn days_to_ymd(days: u64) -> (u64, u64, u64) {
    // Algorithm from http://howardhinnant.github.io/date_algorithms.html
    let z = days + 719468;
    let era = z / 146097;
    let doe = z - era * 146097;
    let yoe = (doe - doe / 1460 + doe / 36524 - doe / 146096) / 365;
    let y = yoe + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = doy - (153 * mp + 2) / 5 + 1;
    let m = if mp < 10 { mp + 3 } else { mp - 9 };
    let y = if m <= 2 { y + 1 } else { y };
    (y, m, d)
}
