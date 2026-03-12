//! Integration tests: in-process worker lifecycle and end-to-end S3 upload.
#![cfg(feature = "worker-s3")]

use aws_config::Region;
use aws_sdk_s3::Client;
use dial9_tokio_telemetry::background_task::BackgroundTaskConfig;
use dial9_tokio_telemetry::background_task::s3::S3Config;
use dial9_tokio_telemetry::telemetry::{RotatingWriter, TracedRuntime};
use flate2::read::GzDecoder;
use std::io::Read;

/// Create an aws_sdk_s3::Client backed by s3s-fs (in-memory fake S3).
///
/// NOTE: This helper is duplicated in src/background_task/s3.rs unit tests.
/// Rust's test compilation model prevents sharing between unit tests (compiled
/// with #[cfg(test)] in src/) and integration tests (compiled from tests/).
/// A shared test-support crate would fix this but is overkill for now.
fn fake_s3_client(fs_root: &std::path::Path) -> aws_sdk_s3::Client {
    let fs = s3s_fs::FileSystem::new(fs_root).unwrap();
    let mut builder = s3s::service::S3ServiceBuilder::new(fs);
    builder.set_auth(s3s::auth::SimpleAuth::from_single("test", "test"));
    let s3_service = builder.build();
    let s3_client: s3s_aws::Client = s3_service.into();

    let s3_config = aws_sdk_s3::Config::builder()
        .behavior_version_latest()
        .credentials_provider(aws_sdk_s3::config::Credentials::new(
            "test", "test", None, None, "test",
        ))
        .region(aws_sdk_s3::config::Region::new("us-east-1"))
        .http_client(s3_client)
        .force_path_style(true)
        .build();

    aws_sdk_s3::Client::from_conf(s3_config)
}

/// Create a dummy S3 config + client for tests that need a BackgroundTaskConfig
/// but don't actually upload anything.
fn dummy_worker_s3(
    trace_path: &std::path::Path,
    s3_root: &std::path::Path,
) -> BackgroundTaskConfig {
    std::fs::create_dir_all(s3_root.join("dummy-bucket")).unwrap();
    let s3_config = S3Config::builder()
        .bucket("dummy-bucket")
        .service_name("test")
        .instance_path("test")
        .boot_id("test")
        .region("us-east-1")
        .build();
    BackgroundTaskConfig::builder()
        .trace_path(trace_path)
        .poll_interval(std::time::Duration::from_millis(50))
        .s3(s3_config)
        .client(fake_s3_client(s3_root))
        .build()
}

/// s3s wrapper that enforces a specific bucket region.
/// `head_bucket` returns the expected region. All other operations reject
/// requests whose `region` field doesn't match, simulating S3's 301 redirect.
struct RegionEnforcingFs<S> {
    inner: S,
    expected_region: String,
}

impl<S> RegionEnforcingFs<S> {
    fn check_region<T>(&self, req: &s3s::S3Request<T>) -> s3s::S3Result<()> {
        match &req.region {
            Some(r) if r.as_str() == self.expected_region => Ok(()),
            other => Err(s3s::S3Error::with_message(
                s3s::S3ErrorCode::PermanentRedirect,
                format!(
                    "wrong region: got {:?}, expected {}",
                    other, self.expected_region
                ),
            )),
        }
    }
}

#[async_trait::async_trait]
impl<S: s3s::S3 + Send + Sync> s3s::S3 for RegionEnforcingFs<S> {
    async fn head_bucket(
        &self,
        _req: s3s::S3Request<s3s::dto::HeadBucketInput>,
    ) -> s3s::S3Result<s3s::S3Response<s3s::dto::HeadBucketOutput>> {
        // Always succeed and report the expected region (no region check here —
        // this is how the client discovers the correct region).
        let output = s3s::dto::HeadBucketOutput {
            bucket_region: Some(self.expected_region.clone()),
            ..Default::default()
        };
        Ok(s3s::S3Response::new(output))
    }

    async fn put_object(
        &self,
        req: s3s::S3Request<s3s::dto::PutObjectInput>,
    ) -> s3s::S3Result<s3s::S3Response<s3s::dto::PutObjectOutput>> {
        self.check_region(&req)?;
        self.inner.put_object(req).await
    }

    async fn get_object(
        &self,
        req: s3s::S3Request<s3s::dto::GetObjectInput>,
    ) -> s3s::S3Result<s3s::S3Response<s3s::dto::GetObjectOutput>> {
        self.check_region(&req)?;
        self.inner.get_object(req).await
    }

    async fn list_objects_v2(
        &self,
        req: s3s::S3Request<s3s::dto::ListObjectsV2Input>,
    ) -> s3s::S3Result<s3s::S3Response<s3s::dto::ListObjectsV2Output>> {
        self.check_region(&req)?;
        self.inner.list_objects_v2(req).await
    }

    async fn create_multipart_upload(
        &self,
        req: s3s::S3Request<s3s::dto::CreateMultipartUploadInput>,
    ) -> s3s::S3Result<s3s::S3Response<s3s::dto::CreateMultipartUploadOutput>> {
        self.check_region(&req)?;
        self.inner.create_multipart_upload(req).await
    }

    async fn upload_part(
        &self,
        req: s3s::S3Request<s3s::dto::UploadPartInput>,
    ) -> s3s::S3Result<s3s::S3Response<s3s::dto::UploadPartOutput>> {
        self.check_region(&req)?;
        self.inner.upload_part(req).await
    }

    async fn complete_multipart_upload(
        &self,
        req: s3s::S3Request<s3s::dto::CompleteMultipartUploadInput>,
    ) -> s3s::S3Result<s3s::S3Response<s3s::dto::CompleteMultipartUploadOutput>> {
        self.check_region(&req)?;
        self.inner.complete_multipart_upload(req).await
    }
}

/// Build an aws_sdk_s3::Client backed by RegionEnforcingFs.
/// The client is intentionally configured with the WRONG region (`us-west-2`).
/// Only requests corrected to `expected_region` will succeed.
fn fake_s3_client_with_region(
    fs_root: &std::path::Path,
    expected_region: &str,
) -> aws_sdk_s3::Client {
    let fs = s3s_fs::FileSystem::new(fs_root).unwrap();
    let region_fs = RegionEnforcingFs {
        inner: fs,
        expected_region: expected_region.to_owned(),
    };
    let mut builder = s3s::service::S3ServiceBuilder::new(region_fs);
    builder.set_auth(s3s::auth::SimpleAuth::from_single("test", "test"));
    let s3_service = builder.build();
    let s3_client: s3s_aws::Client = s3_service.into();

    // Intentionally WRONG region — auto-detection must correct it.
    let s3_config = aws_sdk_s3::Config::builder()
        .behavior_version_latest()
        .credentials_provider(aws_sdk_s3::config::Credentials::new(
            "test", "test", None, None, "test",
        ))
        .region(aws_sdk_s3::config::Region::new("us-west-2"))
        .http_client(s3_client)
        .force_path_style(true)
        .build();

    aws_sdk_s3::Client::from_conf(s3_config)
}

/// s3s wrapper that fails `put_object` calls at a configurable rate.
/// Composes with any inner `S3` impl (e.g. `RegionEnforcingFs<FileSystem>`).
struct FlakyS3<S> {
    inner: S,
    fail_counter: std::sync::atomic::AtomicU64,
    /// Fail every Nth put_object call.
    fail_every_n: u64,
}

#[async_trait::async_trait]
impl<S: s3s::S3 + Send + Sync> s3s::S3 for FlakyS3<S> {
    async fn head_bucket(
        &self,
        req: s3s::S3Request<s3s::dto::HeadBucketInput>,
    ) -> s3s::S3Result<s3s::S3Response<s3s::dto::HeadBucketOutput>> {
        self.inner.head_bucket(req).await
    }

    async fn put_object(
        &self,
        req: s3s::S3Request<s3s::dto::PutObjectInput>,
    ) -> s3s::S3Result<s3s::S3Response<s3s::dto::PutObjectOutput>> {
        let n = self
            .fail_counter
            .fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        if n % self.fail_every_n == 0 {
            return Err(s3s::S3Error::with_message(
                s3s::S3ErrorCode::InternalError,
                "injected failure",
            ));
        }
        self.inner.put_object(req).await
    }

    async fn get_object(
        &self,
        req: s3s::S3Request<s3s::dto::GetObjectInput>,
    ) -> s3s::S3Result<s3s::S3Response<s3s::dto::GetObjectOutput>> {
        self.inner.get_object(req).await
    }

    async fn list_objects_v2(
        &self,
        req: s3s::S3Request<s3s::dto::ListObjectsV2Input>,
    ) -> s3s::S3Result<s3s::S3Response<s3s::dto::ListObjectsV2Output>> {
        self.inner.list_objects_v2(req).await
    }

    async fn create_multipart_upload(
        &self,
        req: s3s::S3Request<s3s::dto::CreateMultipartUploadInput>,
    ) -> s3s::S3Result<s3s::S3Response<s3s::dto::CreateMultipartUploadOutput>> {
        self.inner.create_multipart_upload(req).await
    }

    async fn upload_part(
        &self,
        req: s3s::S3Request<s3s::dto::UploadPartInput>,
    ) -> s3s::S3Result<s3s::S3Response<s3s::dto::UploadPartOutput>> {
        self.inner.upload_part(req).await
    }

    async fn complete_multipart_upload(
        &self,
        req: s3s::S3Request<s3s::dto::CompleteMultipartUploadInput>,
    ) -> s3s::S3Result<s3s::S3Response<s3s::dto::CompleteMultipartUploadOutput>> {
        self.inner.complete_multipart_upload(req).await
    }
}

/// Build an aws_sdk_s3::Client that enforces region AND fails every Nth put_object.
fn fake_s3_client_flaky(
    fs_root: &std::path::Path,
    expected_region: &str,
    fail_every_n: u64,
) -> aws_sdk_s3::Client {
    let fs = s3s_fs::FileSystem::new(fs_root).unwrap();
    let region_fs = RegionEnforcingFs {
        inner: fs,
        expected_region: expected_region.to_owned(),
    };
    let flaky = FlakyS3 {
        inner: region_fs,
        fail_counter: std::sync::atomic::AtomicU64::new(0),
        fail_every_n,
    };
    let mut builder = s3s::service::S3ServiceBuilder::new(flaky);
    builder.set_auth(s3s::auth::SimpleAuth::from_single("test", "test"));
    let s3_service = builder.build();
    let s3_client: s3s_aws::Client = s3_service.into();

    // Intentionally WRONG region — auto-detection must correct it.
    let s3_config = aws_sdk_s3::Config::builder()
        .behavior_version_latest()
        .credentials_provider(aws_sdk_s3::config::Credentials::new(
            "test", "test", None, None, "test",
        ))
        .region(aws_sdk_s3::config::Region::new("us-west-2"))
        .http_client(s3_client)
        .force_path_style(true)
        .build();

    aws_sdk_s3::Client::from_conf(s3_config)
}

#[test]
fn worker_thread_starts_and_stops_cleanly() {
    let trace_dir = tempfile::tempdir().unwrap();
    let s3_root = tempfile::tempdir().unwrap();
    let trace_path = trace_dir.path().join("trace.bin");

    let writer = RotatingWriter::new(&trace_path, 1024, 10 * 1024).unwrap();
    let uploader_config = dummy_worker_s3(&trace_path, s3_root.path());

    let mut builder = tokio::runtime::Builder::new_multi_thread();
    builder.worker_threads(1).enable_all();

    let (runtime, guard) = TracedRuntime::builder()
        .with_s3_uploader(uploader_config)
        .build(builder, writer)
        .unwrap();

    runtime.block_on(async {
        tokio::time::sleep(std::time::Duration::from_millis(100)).await;
    });

    drop(guard);
    drop(runtime);
}

#[tokio::test]
async fn graceful_shutdown_seals_segments() {
    let trace_dir = tempfile::tempdir().unwrap();
    let s3_root = tempfile::tempdir().unwrap();
    let trace_path = trace_dir.path().join("trace.bin");

    let writer = RotatingWriter::new(&trace_path, 1024, 10 * 1024).unwrap();
    let uploader_config = dummy_worker_s3(&trace_path, s3_root.path());

    let mut builder = tokio::runtime::Builder::new_multi_thread();
    builder.worker_threads(1).enable_all();

    let (runtime, guard) = TracedRuntime::builder()
        .with_s3_uploader(uploader_config)
        .build_and_start(builder, writer)
        .unwrap();

    let result = guard
        .graceful_shutdown(std::time::Duration::from_secs(5))
        .await;

    assert!(result.is_ok());

    let active_files: Vec<_> = std::fs::read_dir(trace_dir.path())
        .unwrap()
        .filter_map(|e| e.ok())
        .filter(|e| e.path().extension().is_some_and(|ext| ext == "active"))
        .collect();
    assert!(active_files.is_empty(), "no .active files should remain");

    tokio::task::spawn_blocking(move || drop(runtime))
        .await
        .unwrap();
}

/// End-to-end: TracedRuntime → RotatingWriter → rotation → worker uploads to
/// s3s → download from s3s → decompress → parse with TraceReader → verify
/// real trace events are present.
#[test]
fn end_to_end_trace_to_s3_roundtrip() {
    use dial9_tokio_telemetry::telemetry::analysis::TraceReader;

    let s3_root = tempfile::tempdir().unwrap();
    let trace_dir = tempfile::tempdir().unwrap();
    let trace_path = trace_dir.path().join("trace.bin");

    // Create the bucket directory for s3s-fs
    std::fs::create_dir(s3_root.path().join("test-bucket")).unwrap();

    let client = fake_s3_client(s3_root.path());

    // Small max_file_size to force rotation quickly
    let writer = RotatingWriter::new(&trace_path, 512, 50 * 1024).unwrap();

    let s3_config = S3Config::builder()
        .bucket("test-bucket")
        .prefix("traces")
        .service_name("test-svc")
        .instance_path("us-east-1/test-host")
        .boot_id("test-boot-id")
        .region("us-east-1")
        .build();

    let uploader_config = BackgroundTaskConfig::builder()
        .trace_path(&trace_path)
        .poll_interval(std::time::Duration::from_millis(50))
        .s3(s3_config)
        .client(client.clone())
        .build();

    let mut builder = tokio::runtime::Builder::new_multi_thread();
    builder.worker_threads(2).enable_all();

    let (runtime, guard) = TracedRuntime::builder()
        .with_s3_uploader(uploader_config)
        .build_and_start(builder, writer)
        .unwrap();

    // Run a workload that generates enough events to trigger rotation.
    runtime.block_on(async {
        let mut handles = Vec::new();
        for _ in 0..50 {
            handles.push(tokio::spawn(async {
                tokio::task::yield_now().await;
            }));
        }
        for h in handles {
            let _ = h.await;
        }
        // Give the flush thread time to write events and the worker time to upload
        tokio::time::sleep(std::time::Duration::from_secs(2)).await;
    });

    // Drop guard: stops flush, seals final segment, worker drains to S3
    drop(guard);
    drop(runtime);

    // List objects in the bucket — should have at least one uploaded segment
    let list_rt = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .unwrap();

    let objects = list_rt.block_on(async {
        let resp = client
            .list_objects_v2()
            .bucket("test-bucket")
            .prefix("traces/")
            .send()
            .await
            .unwrap();
        resp.contents.unwrap_or_default()
    });

    assert!(
        !objects.is_empty(),
        "expected at least one object in S3, got none"
    );

    // Download the first object, decompress, write to temp file, parse
    let first_key = objects[0].key().unwrap().to_string();
    eprintln!("downloaded key: {first_key}");

    let downloaded_path = trace_dir.path().join("downloaded.bin");

    list_rt.block_on(async {
        let resp = client
            .get_object()
            .bucket("test-bucket")
            .key(&first_key)
            .send()
            .await
            .unwrap();

        let body = resp.body.collect().await.unwrap().into_bytes();

        // Decompress gzip
        let mut decoder = GzDecoder::new(&body[..]);
        let mut decompressed = Vec::new();
        decoder.read_to_end(&mut decompressed).unwrap();

        std::fs::write(&downloaded_path, &decompressed).unwrap();
    });

    // Parse the downloaded trace with TraceReader
    let mut reader = TraceReader::new(downloaded_path.to_str().unwrap()).unwrap();
    let (_magic, version) = reader.read_header().unwrap();
    assert!(version > 0, "expected valid format version");

    let events = reader.read_all().unwrap();
    assert!(
        !events.is_empty(),
        "expected trace events in downloaded segment, got none"
    );

    // Should contain at least some PollStart/PollEnd or WorkerPark events
    let has_runtime_events = events.iter().any(|e| e.timestamp_nanos().is_some());
    assert!(
        has_runtime_events,
        "expected runtime events with timestamps, found none in {} events",
        events.len()
    );

    eprintln!(
        "end-to-end success: {} objects in S3, first has {} events (format v{})",
        objects.len(),
        events.len(),
        version
    );
}

/// Verify that the worker auto-detects the bucket region from HeadBucket
/// and corrects the client, even when the initial client has the wrong region.
#[test]
fn region_auto_detection_corrects_wrong_client_region() {
    use dial9_tokio_telemetry::telemetry::analysis::TraceReader;

    let s3_root = tempfile::tempdir().unwrap();
    let trace_dir = tempfile::tempdir().unwrap();
    let trace_path = trace_dir.path().join("trace.bin");

    std::fs::create_dir(s3_root.path().join("test-bucket")).unwrap();

    // Client for the worker: wrong region, must auto-detect.
    let client = fake_s3_client_with_region(s3_root.path(), "eu-west-1");

    // Separate client for test verification: correct region.
    let verify_client = Client::from_conf(
        client
            .config()
            .to_builder()
            .region(Region::from_static("eu-west-1"))
            .build(),
    );

    let writer = RotatingWriter::new(&trace_path, 512, 50 * 1024).unwrap();

    // Do NOT set .region() — force auto-detection.
    let s3_config = S3Config::builder()
        .bucket("test-bucket")
        .prefix("traces")
        .service_name("test-svc")
        .instance_path("test-host")
        .boot_id("test-boot-id")
        .build();

    let uploader_config = BackgroundTaskConfig::builder()
        .trace_path(&trace_path)
        .poll_interval(std::time::Duration::from_millis(50))
        .s3(s3_config)
        .client(client.clone())
        .build();

    let mut builder = tokio::runtime::Builder::new_multi_thread();
    builder.worker_threads(2).enable_all();

    let (runtime, guard) = TracedRuntime::builder()
        .with_s3_uploader(uploader_config)
        .build_and_start(builder, writer)
        .unwrap();

    runtime.block_on(async {
        for _ in 0..50 {
            tokio::spawn(async { tokio::task::yield_now().await });
        }
        tokio::time::sleep(std::time::Duration::from_secs(2)).await;
    });

    drop(guard);
    drop(runtime);

    // Verify objects were uploaded despite the wrong initial region.
    let list_rt = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .unwrap();

    let objects = list_rt.block_on(async {
        let resp = verify_client
            .list_objects_v2()
            .bucket("test-bucket")
            .prefix("traces/")
            .send()
            .await
            .unwrap();
        resp.contents.unwrap_or_default()
    });

    assert!(
        !objects.is_empty(),
        "expected uploads to succeed after region auto-detection"
    );

    // Download and verify the trace is parseable.
    let first_key = objects[0].key().unwrap().to_string();
    let downloaded_path = trace_dir.path().join("downloaded.bin");

    list_rt.block_on(async {
        let resp = verify_client
            .get_object()
            .bucket("test-bucket")
            .key(&first_key)
            .send()
            .await
            .unwrap();
        let body = resp.body.collect().await.unwrap().into_bytes();
        let mut decoder = GzDecoder::new(&body[..]);
        let mut decompressed = Vec::new();
        decoder.read_to_end(&mut decompressed).unwrap();
        std::fs::write(&downloaded_path, &decompressed).unwrap();
    });

    let mut reader = TraceReader::new(downloaded_path.to_str().unwrap()).unwrap();
    reader.read_header().unwrap();
    let events = reader.read_all().unwrap();
    assert!(
        !events.is_empty(),
        "expected trace events after region correction"
    );
}

/// Stress test: generate high-throughput trace data against a local S3 server
/// and verify invariants.
///
/// Invariants checked:
/// 1. All segments uploaded — no data left on disk after graceful shutdown
/// 2. Every uploaded object is valid gzip containing parseable trace events
/// 3. Compression ratio is sane (compressed < uncompressed)
/// 4. Segment indices are sorted with no duplicates (gaps expected from eviction)
/// 5. Total events across all segments is non-trivial
/// 6. Worker metrics match: success count == object count, sizes non-zero, stages succeed
#[test]
fn stress_test_all_segments_uploaded_and_valid() {
    use dial9_tokio_telemetry::telemetry::analysis::TraceReader;

    let s3_root = tempfile::tempdir().unwrap();
    let trace_dir = tempfile::tempdir().unwrap();
    let trace_path = trace_dir.path().join("trace.bin");

    std::fs::create_dir(s3_root.path().join("stress-bucket")).unwrap();
    let client = fake_s3_client(s3_root.path());

    // Small segments (64KB) to force many rotations under load.
    let segment_size = 64 * 1024;
    let total_size = 10 * 1024 * 1024; // 10 MB disk budget
    let writer = RotatingWriter::new(&trace_path, segment_size, total_size).unwrap();

    let s3_config = S3Config::builder()
        .bucket("stress-bucket")
        .prefix("traces")
        .service_name("stress-svc")
        .instance_path("test-host")
        .boot_id("stress-boot")
        .region("us-east-1")
        .build();

    let metrique_writer::test_util::TestEntrySink { inspector, sink: metrics_sink } =
        metrique_writer::test_util::test_entry_sink();

    let uploader_config = BackgroundTaskConfig::builder()
        .trace_path(&trace_path)
        .poll_interval(std::time::Duration::from_millis(50))
        .s3(s3_config)
        .client(client.clone())
        .metrics_sink(metrics_sink)
        .build();

    let mut builder = tokio::runtime::Builder::new_multi_thread();
    builder.worker_threads(4).enable_all();

    let (runtime, guard) = TracedRuntime::builder()
        .with_task_tracking(true)
        .with_s3_uploader(uploader_config)
        .build_and_start(builder, writer)
        .unwrap();

    let handle = guard.handle();

    // Generate load for 3 seconds — enough to produce many segments at 64KB each.
    runtime.block_on(async {
        let deadline = tokio::time::Instant::now() + std::time::Duration::from_secs(3);
        loop {
            if tokio::time::Instant::now() >= deadline {
                break;
            }
            let mut joins = Vec::with_capacity(100);
            for _ in 0..100 {
                joins.push(handle.spawn(async {
                    tokio::task::yield_now().await;
                    tokio::task::yield_now().await;
                }));
            }
            for j in joins {
                let _ = j.await;
            }
        }

        // Graceful shutdown: seals final segment, worker drains to S3.
        guard
            .graceful_shutdown(std::time::Duration::from_secs(60))
            .await
            .expect("graceful shutdown");
    });

    drop(runtime);

    // Invariant 1: no sealed .bin files left on disk (all uploaded + deleted).
    let leftover_bins: Vec<_> = std::fs::read_dir(trace_dir.path())
        .unwrap()
        .filter_map(|e| e.ok())
        .filter(|e| {
            let name = e.file_name();
            let name = name.to_string_lossy();
            name.starts_with("trace.") && name.ends_with(".bin") && !name.ends_with(".active")
        })
        .collect();
    assert!(
        leftover_bins.is_empty(),
        "expected all segments uploaded and deleted, but found {} leftover files: {:?}",
        leftover_bins.len(),
        leftover_bins
            .iter()
            .map(|e| e.file_name())
            .collect::<Vec<_>>()
    );

    // List all uploaded objects.
    let list_rt = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .unwrap();

    let objects = list_rt.block_on(async {
        let mut objects = Vec::new();
        let mut continuation: Option<String> = None;
        loop {
            let mut req = client
                .list_objects_v2()
                .bucket("stress-bucket")
                .prefix("traces/");
            if let Some(token) = continuation.take() {
                req = req.continuation_token(token);
            }
            let resp = req.send().await.unwrap();
            for obj in resp.contents() {
                objects.push(obj.key().unwrap().to_string());
            }
            if resp.is_truncated() == Some(true) {
                continuation = resp.next_continuation_token().map(|s| s.to_string());
            } else {
                break;
            }
        }
        objects
    });

    assert!(
        objects.len() >= 5,
        "expected many uploaded segments, got {}",
        objects.len()
    );
    eprintln!("stress test: {} objects uploaded to S3", objects.len());

    // Download and validate every object.
    let mut total_events = 0usize;
    let mut total_compressed = 0u64;
    let mut total_uncompressed = 0u64;

    for key in &objects {
        assert!(key.ends_with(".bin.gz"), "unexpected key suffix: {key}");

        let (decompressed, compressed_size) = list_rt.block_on(async {
            let resp = client
                .get_object()
                .bucket("stress-bucket")
                .key(key)
                .send()
                .await
                .unwrap();
            let body = resp.body.collect().await.unwrap().into_bytes();
            let compressed_size = body.len() as u64;

            // Invariant 2: valid gzip.
            let mut decoder = GzDecoder::new(&body[..]);
            let mut decompressed = Vec::new();
            decoder
                .read_to_end(&mut decompressed)
                .unwrap_or_else(|e| panic!("failed to decompress {key}: {e}"));
            (decompressed, compressed_size)
        });

        // Invariant 3: compression ratio is sane.
        let uncompressed_size = decompressed.len() as u64;
        assert!(
            compressed_size < uncompressed_size,
            "compressed ({compressed_size}) should be smaller than uncompressed ({uncompressed_size}) for {key}"
        );
        total_compressed += compressed_size;
        total_uncompressed += uncompressed_size;

        // Invariant 2 continued: parseable trace events.
        let tmp = tempfile::NamedTempFile::new().unwrap();
        std::fs::write(tmp.path(), &decompressed).unwrap();
        let mut reader = TraceReader::new(tmp.path().to_str().unwrap()).unwrap();
        let (_magic, version) = reader.read_header().unwrap();
        assert!(version > 0, "invalid format version in {key}");
        let events = reader.read_all().unwrap();
        assert!(!events.is_empty(), "expected events in {key}, got none");
        total_events += events.len();
    }

    // Invariant 5: non-trivial total event count.
    assert!(
        total_events > 1000,
        "expected many events across all segments, got {total_events}"
    );

    // Invariant 4: segment indices are sorted with no duplicates.
    // Gaps are expected when the disk budget evicts segments faster than the
    // worker can upload them.
    let mut segment_indices: Vec<u32> = objects
        .iter()
        .filter_map(|key| {
            let filename = key.rsplit('/').next()?;
            let stem = filename.strip_suffix(".bin.gz")?;
            let idx_str = stem.rsplit('-').next()?;
            idx_str.parse().ok()
        })
        .collect();
    segment_indices.sort();
    let before_dedup = segment_indices.len();
    segment_indices.dedup();
    assert_eq!(
        segment_indices.len(),
        before_dedup,
        "segment indices should have no duplicates, but found {} duplicates",
        before_dedup - segment_indices.len(),
    );

    // Invariant 6: worker metrics are consistent with uploaded objects.
    let entries = inspector.entries();
    let successes: Vec<_> = entries
        .iter()
        .filter(|e| e.metrics["Success"].as_u64() == 1)
        .collect();
    assert_eq!(
        successes.len(),
        objects.len(),
        "metric success count ({}) should match uploaded object count ({})",
        successes.len(),
        objects.len(),
    );
    for entry in &successes {
        let compressed = entry.metrics["CompressedSize"].as_u64();
        let uncompressed = entry.metrics["UncompressedSize"].as_u64();
        assert!(compressed > 0, "CompressedSize should be non-zero");
        assert!(uncompressed > 0, "UncompressedSize should be non-zero");
        assert!(
            compressed < uncompressed,
            "compressed ({compressed}) should be < uncompressed ({uncompressed})"
        );
        assert!(
            entry.metrics["Gzip.Success"].as_u64() == 1,
            "Gzip stage should succeed"
        );
        assert!(
            entry.metrics["S3Upload.Success"].as_u64() == 1,
            "S3Upload stage should succeed"
        );
    }

    let ratio = total_uncompressed as f64 / total_compressed as f64;
    eprintln!(
        "stress test passed: {} objects, {} total events, {:.1}MB uncompressed, {:.1}MB compressed, {:.1}:1 ratio",
        objects.len(),
        total_events,
        total_uncompressed as f64 / 1_000_000.0,
        total_compressed as f64 / 1_000_000.0,
        ratio,
    );
}

/// s3s wrapper where `put_object` hangs forever (the future never resolves).
/// All other operations delegate to the inner impl.
struct HangingS3<S> {
    inner: S,
}

#[async_trait::async_trait]
impl<S: s3s::S3 + Send + Sync> s3s::S3 for HangingS3<S> {
    async fn head_bucket(
        &self,
        req: s3s::S3Request<s3s::dto::HeadBucketInput>,
    ) -> s3s::S3Result<s3s::S3Response<s3s::dto::HeadBucketOutput>> {
        self.inner.head_bucket(req).await
    }

    async fn put_object(
        &self,
        _req: s3s::S3Request<s3s::dto::PutObjectInput>,
    ) -> s3s::S3Result<s3s::S3Response<s3s::dto::PutObjectOutput>> {
        // Hang forever — simulates an S3 call that never completes.
        std::future::pending().await
    }

    async fn get_object(
        &self,
        req: s3s::S3Request<s3s::dto::GetObjectInput>,
    ) -> s3s::S3Result<s3s::S3Response<s3s::dto::GetObjectOutput>> {
        self.inner.get_object(req).await
    }

    async fn list_objects_v2(
        &self,
        req: s3s::S3Request<s3s::dto::ListObjectsV2Input>,
    ) -> s3s::S3Result<s3s::S3Response<s3s::dto::ListObjectsV2Output>> {
        self.inner.list_objects_v2(req).await
    }

    async fn create_multipart_upload(
        &self,
        req: s3s::S3Request<s3s::dto::CreateMultipartUploadInput>,
    ) -> s3s::S3Result<s3s::S3Response<s3s::dto::CreateMultipartUploadOutput>> {
        self.inner.create_multipart_upload(req).await
    }

    async fn upload_part(
        &self,
        req: s3s::S3Request<s3s::dto::UploadPartInput>,
    ) -> s3s::S3Result<s3s::S3Response<s3s::dto::UploadPartOutput>> {
        self.inner.upload_part(req).await
    }

    async fn complete_multipart_upload(
        &self,
        req: s3s::S3Request<s3s::dto::CompleteMultipartUploadInput>,
    ) -> s3s::S3Result<s3s::S3Response<s3s::dto::CompleteMultipartUploadOutput>> {
        self.inner.complete_multipart_upload(req).await
    }
}

fn fake_s3_client_hanging(fs_root: &std::path::Path) -> aws_sdk_s3::Client {
    let fs = s3s_fs::FileSystem::new(fs_root).unwrap();
    let hanging = HangingS3 { inner: fs };
    let mut builder = s3s::service::S3ServiceBuilder::new(hanging);
    builder.set_auth(s3s::auth::SimpleAuth::from_single("test", "test"));
    let s3_service = builder.build();
    let s3_client: s3s_aws::Client = s3_service.into();

    let s3_config = aws_sdk_s3::Config::builder()
        .behavior_version_latest()
        .credentials_provider(aws_sdk_s3::config::Credentials::new(
            "test", "test", None, None, "test",
        ))
        .region(aws_sdk_s3::config::Region::new("us-east-1"))
        .http_client(s3_client)
        .force_path_style(true)
        .build();

    aws_sdk_s3::Client::from_conf(s3_config)
}

/// When S3 hangs permanently (put_object never returns), graceful_shutdown
/// must still complete within its timeout instead of blocking forever.
#[tokio::test]
async fn graceful_shutdown_completes_when_s3_hangs() {
    let trace_dir = tempfile::tempdir().unwrap();
    let s3_root = tempfile::tempdir().unwrap();
    let trace_path = trace_dir.path().join("trace.bin");

    std::fs::create_dir_all(s3_root.path().join("hang-bucket")).unwrap();
    let client = fake_s3_client_hanging(s3_root.path());

    // Small segments to force rotation quickly.
    let writer = RotatingWriter::new(&trace_path, 512, 50 * 1024).unwrap();

    let s3_config = S3Config::builder()
        .bucket("hang-bucket")
        .prefix("traces")
        .service_name("test-svc")
        .instance_path("test-host")
        .boot_id("test-boot")
        .region("us-east-1")
        .build();

    let uploader_config = BackgroundTaskConfig::builder()
        .trace_path(&trace_path)
        .poll_interval(std::time::Duration::from_millis(50))
        .s3(s3_config)
        .client(client)
        .build();

    let mut builder = tokio::runtime::Builder::new_multi_thread();
    builder.worker_threads(2).enable_all();

    let (runtime, guard) = TracedRuntime::builder()
        .with_s3_uploader(uploader_config)
        .build_and_start(builder, writer)
        .unwrap();

    // Generate trace data on the TracedRuntime, then let the worker pick it up.
    let handle = guard.handle();
    let rt_handle = runtime.handle().clone();
    tokio::task::spawn_blocking(move || {
        rt_handle.block_on(async {
            for _ in 0..50 {
                handle.spawn(async { tokio::task::yield_now().await });
            }
            tokio::time::sleep(std::time::Duration::from_secs(1)).await;
        });
    })
    .await
    .unwrap();

    // graceful_shutdown should complete within the timeout, not hang forever.
    // We use a test-level timeout to detect the hang.
    let shutdown_timeout = std::time::Duration::from_secs(3);
    let test_deadline = std::time::Duration::from_secs(10);

    let result = tokio::time::timeout(test_deadline, guard.graceful_shutdown(shutdown_timeout)).await;

    // If the test-level timeout fires, graceful_shutdown hung — that's the bug.
    assert!(
        result.is_ok(),
        "graceful_shutdown hung beyond {test_deadline:?} — it did not respect its own {shutdown_timeout:?} timeout"
    );

    eprintln!("hanging S3 test: graceful_shutdown returned {:?}", result.unwrap());

    tokio::task::spawn_blocking(move || drop(runtime))
        .await
        .unwrap();
}

/// Stress test with injected S3 failures.
///
/// Same as `stress_test_all_segments_uploaded_and_valid` but every 3rd
/// `put_object` call returns InternalError. The worker must retry failed
/// segments and eventually upload everything. Metrics must reflect both
/// successes and failures.
#[test]
fn stress_test_with_s3_failures() {
    let s3_root = tempfile::tempdir().unwrap();
    let trace_dir = tempfile::tempdir().unwrap();
    let trace_path = trace_dir.path().join("trace.bin");

    std::fs::create_dir(s3_root.path().join("flaky-bucket")).unwrap();
    // Fail every 3rd put_object; client configured with wrong region too.
    let client = fake_s3_client_flaky(s3_root.path(), "us-east-1", 3);

    let segment_size = 64 * 1024;
    let total_size = 10 * 1024 * 1024;
    let writer = RotatingWriter::new(&trace_path, segment_size, total_size).unwrap();

    let s3_config = S3Config::builder()
        .bucket("flaky-bucket")
        .prefix("traces")
        .service_name("flaky-svc")
        .instance_path("test-host")
        .boot_id("flaky-boot")
        .region("us-east-1")
        .build();

    let metrique_writer::test_util::TestEntrySink { inspector, sink: metrics_sink } =
        metrique_writer::test_util::test_entry_sink();

    let uploader_config = BackgroundTaskConfig::builder()
        .trace_path(&trace_path)
        .poll_interval(std::time::Duration::from_millis(50))
        .s3(s3_config)
        .client(client.clone())
        .metrics_sink(metrics_sink)
        .build();

    let mut builder = tokio::runtime::Builder::new_multi_thread();
    builder.worker_threads(4).enable_all();

    let (runtime, guard) = TracedRuntime::builder()
        .with_task_tracking(true)
        .with_s3_uploader(uploader_config)
        .build_and_start(builder, writer)
        .unwrap();

    let handle = guard.handle();

    runtime.block_on(async {
        let deadline = tokio::time::Instant::now() + std::time::Duration::from_secs(3);
        loop {
            if tokio::time::Instant::now() >= deadline {
                break;
            }
            let mut joins = Vec::with_capacity(100);
            for _ in 0..100 {
                joins.push(handle.spawn(async {
                    tokio::task::yield_now().await;
                    tokio::task::yield_now().await;
                }));
            }
            for j in joins {
                let _ = j.await;
            }
        }

        guard
            .graceful_shutdown(std::time::Duration::from_secs(5))
            .await
            .expect("graceful shutdown");
    });

    drop(runtime);

    // Worker metrics should show both successes and failures.
    let entries = inspector.entries();
    let successes = entries
        .iter()
        .filter(|e| e.metrics["Success"].as_u64() == 1)
        .count();
    let failures = entries
        .iter()
        .filter(|e| e.metrics["Success"].as_u64() == 0)
        .count();

    eprintln!(
        "flaky stress test: {} metric entries, {} successes, {} failures",
        entries.len(),
        successes,
        failures,
    );

    // With 1-in-3 failure rate, we must see some failures.
    assert!(
        failures > 0,
        "expected some S3 failures from injected errors, got 0 failures out of {} entries",
        entries.len(),
    );
    // And some successes — the worker retries and eventually uploads.
    assert!(
        successes > 0,
        "expected some successful uploads despite failures, got 0 successes out of {} entries",
        entries.len(),
    );

    // Every success should have valid compressed/uncompressed sizes.
    for entry in entries.iter().filter(|e| e.metrics["Success"].as_u64() == 1) {
        let compressed = entry.metrics["CompressedSize"].as_u64();
        let uncompressed = entry.metrics["UncompressedSize"].as_u64();
        assert!(compressed > 0, "CompressedSize should be non-zero on success");
        assert!(
            compressed < uncompressed,
            "compressed ({compressed}) should be < uncompressed ({uncompressed})"
        );
    }

    // Verify objects actually landed in S3.
    let list_rt = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .unwrap();
    let object_count = list_rt.block_on(async {
        let resp = client
            .list_objects_v2()
            .bucket("flaky-bucket")
            .prefix("traces/")
            .send()
            .await
            .unwrap();
        resp.key_count.unwrap_or(0)
    });

    assert_eq!(
        object_count as usize, successes,
        "S3 object count should match metric success count"
    );

    eprintln!(
        "flaky stress test passed: {} objects in S3, {} failures absorbed",
        object_count, failures,
    );
}
