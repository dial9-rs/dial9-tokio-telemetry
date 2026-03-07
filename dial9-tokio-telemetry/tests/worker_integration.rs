//! Integration tests: in-process worker lifecycle and end-to-end S3 upload.
#![cfg(feature = "worker-s3")]

use dial9_tokio_telemetry::telemetry::{RotatingWriter, TracedRuntime};
use dial9_tokio_telemetry::worker::s3::S3Config;
use dial9_tokio_telemetry::worker::WorkerConfig;
use flate2::read::GzDecoder;
use std::io::Read;

/// Create a transfer manager Client backed by s3s-fs.
///
/// NOTE: These helpers are duplicated in src/worker/s3.rs unit tests.
/// Rust's test compilation model prevents sharing between unit tests (compiled
/// with #[cfg(test)] in src/) and integration tests (compiled from tests/).
/// A shared test-support crate would fix this but is overkill for now.
fn fake_s3_client(fs_root: &std::path::Path) -> aws_sdk_s3_transfer_manager::Client {
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

    let sdk_client = aws_sdk_s3::Client::from_conf(s3_config);

    let tm_config = aws_sdk_s3_transfer_manager::Config::builder()
        .client(sdk_client)
        .build();

    aws_sdk_s3_transfer_manager::Client::new(tm_config)
}

/// Create a raw aws_sdk_s3::Client for reading back objects.
fn fake_raw_s3_client(fs_root: &std::path::Path) -> aws_sdk_s3::Client {
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

/// Create a dummy S3 config + client for tests that need a WorkerConfig
/// but don't actually upload anything.
fn dummy_worker_s3(
    trace_path: &std::path::Path,
    s3_root: &std::path::Path,
) -> WorkerConfig {
    std::fs::create_dir_all(s3_root.join("dummy-bucket")).unwrap();
    let s3_config = S3Config::builder()
        .bucket("dummy-bucket")
        .service_name("test")
        .instance_path("test")
        .boot_id("test")
        .build();
    WorkerConfig::builder()
        .trace_path(trace_path)
        .poll_interval(std::time::Duration::from_millis(50))
        .s3(s3_config)
        .client(fake_s3_client(s3_root))
        .build()
}

#[test]
fn worker_thread_starts_and_stops_cleanly() {
    let trace_dir = tempfile::tempdir().unwrap();
    let s3_root = tempfile::tempdir().unwrap();
    let trace_path = trace_dir.path().join("trace.bin");

    let writer = RotatingWriter::new(&trace_path, 1024, 10 * 1024).unwrap();
    let worker_config = dummy_worker_s3(&trace_path, s3_root.path());

    let mut builder = tokio::runtime::Builder::new_multi_thread();
    builder.worker_threads(1).enable_all();

    let (runtime, guard) = TracedRuntime::builder()
        .in_process_worker(worker_config)
        .build(builder, writer)
        .unwrap();

    runtime.block_on(async {
        tokio::time::sleep(std::time::Duration::from_millis(100)).await;
    });

    drop(guard);
    drop(runtime);
}

#[tokio::test]
async fn graceful_shutdown_writes_sentinel_and_seals() {
    let trace_dir = tempfile::tempdir().unwrap();
    let s3_root = tempfile::tempdir().unwrap();
    let trace_path = trace_dir.path().join("trace.bin");

    let writer = RotatingWriter::new(&trace_path, 1024, 10 * 1024).unwrap();
    let worker_config = dummy_worker_s3(&trace_path, s3_root.path());

    let mut builder = tokio::runtime::Builder::new_multi_thread();
    builder.worker_threads(1).enable_all();

    let (runtime, guard) = TracedRuntime::builder()
        .in_process_worker(worker_config)
        .build_and_start(builder, writer)
        .unwrap();

    let result = guard
        .graceful_shutdown(std::time::Duration::from_secs(5))
        .await;

    assert!(result.is_ok());
    assert!(trace_dir.path().join(".shutdown").exists());

    let active_files: Vec<_> = std::fs::read_dir(trace_dir.path())
        .unwrap()
        .filter_map(|e| e.ok())
        .filter(|e| {
            e.path()
                .extension()
                .map_or(false, |ext| ext == "active")
        })
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

    let tm_client = fake_s3_client(s3_root.path());
    let raw_client = fake_raw_s3_client(s3_root.path());

    // Small max_file_size to force rotation quickly
    let writer = RotatingWriter::new(&trace_path, 512, 50 * 1024).unwrap();

    let s3_config = S3Config::builder()
        .bucket("test-bucket")
        .prefix("traces")
        .service_name("test-svc")
        .instance_path("us-east-1/test-host")
        .boot_id("test-boot-id")
        .build();

    let worker_config = WorkerConfig::builder()
        .trace_path(&trace_path)
        .poll_interval(std::time::Duration::from_millis(50))
        .s3(s3_config)
        .client(tm_client)
        .build();

    let mut builder = tokio::runtime::Builder::new_multi_thread();
    builder.worker_threads(2).enable_all();

    let (runtime, guard) = TracedRuntime::builder()
        .in_process_worker(worker_config)
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
        let resp = raw_client
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
        let resp = raw_client
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
