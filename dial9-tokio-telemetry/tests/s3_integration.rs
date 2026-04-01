//! Integration tests: in-process worker lifecycle and end-to-end S3 upload.
#![cfg(feature = "worker-s3")]

mod fake_s3;

use aws_config::Region;
use aws_sdk_s3::Client;
use dial9_tokio_telemetry::background_task::s3::S3Config;
use dial9_tokio_telemetry::telemetry::{RotatingWriter, TracedRuntime};
use fake_s3::{
    fake_s3_client, fake_s3_client_always_failing, fake_s3_client_flaky, fake_s3_client_hanging,
    fake_s3_client_with_region,
};
use flate2::read::GzDecoder;
use std::io::Read;

/// Create a dummy S3 config + client for tests.
fn dummy_s3(s3_root: &std::path::Path) -> (S3Config, aws_sdk_s3::Client) {
    std::fs::create_dir_all(s3_root.join("dummy-bucket")).unwrap();
    let s3_config = S3Config::builder()
        .bucket("dummy-bucket")
        .service_name("test")
        .instance_path("test")
        .boot_id("test")
        .region("us-east-1")
        .build();
    (s3_config, fake_s3_client(s3_root))
}

#[test]
fn worker_thread_starts_and_stops_cleanly() {
    let trace_dir = tempfile::tempdir().unwrap();
    let s3_root = tempfile::tempdir().unwrap();
    let trace_path = trace_dir.path().join("trace.bin");

    let writer = RotatingWriter::new(&trace_path, 1024, 10 * 1024).unwrap();
    let (s3_config, client) = dummy_s3(s3_root.path());

    let mut builder = tokio::runtime::Builder::new_multi_thread();
    builder.worker_threads(1).enable_all();

    let (runtime, guard) = TracedRuntime::builder()
        .with_trace_path(&trace_path)
        .with_s3_uploader(s3_config.clone())
        .with_s3_client(client.clone())
        .with_worker_poll_interval(std::time::Duration::from_millis(50))
        .build(builder, writer)
        .unwrap();

    runtime.block_on(async {
        tokio::time::sleep(std::time::Duration::from_millis(100)).await;
    });

    drop(guard);
    drop(runtime);
}

#[test]
fn graceful_shutdown_seals_segments() {
    let trace_dir = tempfile::tempdir().unwrap();
    let s3_root = tempfile::tempdir().unwrap();
    let trace_path = trace_dir.path().join("trace.bin");

    let writer = RotatingWriter::new(&trace_path, 1024, 10 * 1024).unwrap();
    let (s3_config, client) = dummy_s3(s3_root.path());

    let mut builder = tokio::runtime::Builder::new_multi_thread();
    builder.worker_threads(1).enable_all();

    let (runtime, guard) = TracedRuntime::builder()
        .with_trace_path(&trace_path)
        .with_s3_uploader(s3_config.clone())
        .with_s3_client(client.clone())
        .with_worker_poll_interval(std::time::Duration::from_millis(50))
        .build_and_start(builder, writer)
        .unwrap();

    drop(runtime);
    let result = guard.graceful_shutdown(std::time::Duration::from_secs(5));

    assert!(result.is_ok());

    let active_files: Vec<_> = std::fs::read_dir(trace_dir.path())
        .unwrap()
        .filter_map(|e| e.ok())
        .filter(|e| e.path().extension().is_some_and(|ext| ext == "active"))
        .collect();
    assert!(active_files.is_empty(), "no .active files should remain");
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

    let mut builder = tokio::runtime::Builder::new_multi_thread();
    builder.worker_threads(2).enable_all();

    let (runtime, guard) = TracedRuntime::builder()
        .with_trace_path(&trace_path)
        .with_s3_uploader(s3_config.clone())
        .with_s3_client(client.clone())
        .with_worker_poll_interval(std::time::Duration::from_millis(50))
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

    let mut builder = tokio::runtime::Builder::new_multi_thread();
    builder.worker_threads(2).enable_all();

    let (runtime, guard) = TracedRuntime::builder()
        .with_trace_path(&trace_path)
        .with_s3_uploader(s3_config.clone())
        .with_s3_client(client.clone())
        .with_worker_poll_interval(std::time::Duration::from_millis(50))
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
/// 1. Every uploaded object is valid gzip containing parseable trace events
/// 2. Compression ratio is sane (compressed < uncompressed)
/// 3. Segment indices are sorted with no duplicates (gaps expected from eviction)
/// 4. Total events across all segments is non-trivial
/// 5. Worker metrics match: success count == object count, sizes non-zero, stages succeed
///
/// Note: some segments may remain on disk after shutdown — the worker drains
/// what it can within the timeout but won't block the application. On restart,
/// the worker would pick up any leftover segments.
#[test]
fn stress_test_all_segments_uploaded_and_valid() {
    use dial9_tokio_telemetry::telemetry::analysis::TraceReader;

    let s3_root = tempfile::tempdir().unwrap();
    let trace_dir = tempfile::tempdir().unwrap();
    let trace_path = trace_dir.path().join("trace.bin");

    std::fs::create_dir(s3_root.path().join("stress-bucket")).unwrap();
    let client = fake_s3_client(s3_root.path());

    // Small segments to force rotations, but not so many that drain takes forever.
    let segment_size = 64 * 1024;
    let total_size = 2 * 1024 * 1024; // 2 MB disk budget
    let writer = RotatingWriter::new(&trace_path, segment_size, total_size).unwrap();

    let s3_config = S3Config::builder()
        .bucket("stress-bucket")
        .prefix("traces")
        .service_name("stress-svc")
        .instance_path("test-host")
        .boot_id("stress-boot")
        .region("us-east-1")
        .build();

    let metrique_writer::test_util::TestEntrySink {
        inspector,
        sink: metrics_sink,
    } = metrique_writer::test_util::test_entry_sink();

    let mut builder = tokio::runtime::Builder::new_multi_thread();
    builder.worker_threads(4).enable_all();

    let (runtime, guard) = TracedRuntime::builder()
        .with_task_tracking(true)
        .with_trace_path(&trace_path)
        .with_s3_uploader(s3_config.clone())
        .with_s3_client(client.clone())
        .with_worker_poll_interval(std::time::Duration::from_millis(50))
        .with_worker_metrics_sink(metrics_sink)
        .build_and_start(builder, writer)
        .unwrap();

    let handle = guard.handle();

    // Generate load for 1 second — enough to produce several segments at 64KB each.
    runtime.block_on(async {
        let deadline = tokio::time::Instant::now() + std::time::Duration::from_secs(1);
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

        // Graceful shutdown: seals final segment, worker drains what it can
        // within the timeout. Some segments may remain — the worker is a "good
        // citizen" that loses data rather than blocking the application.
    });

    drop(runtime);
    guard
        .graceful_shutdown(std::time::Duration::from_secs(10))
        .expect("graceful shutdown");

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

    // Download and validate every object.
    let mut total_events = 0usize;

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

            let mut decoder = GzDecoder::new(&body[..]);
            let mut decompressed = Vec::new();
            decoder
                .read_to_end(&mut decompressed)
                .unwrap_or_else(|e| panic!("failed to decompress {key}: {e}"));
            (decompressed, compressed_size)
        });

        // Compression ratio is sane.
        let uncompressed_size = decompressed.len() as u64;
        assert!(
            compressed_size < uncompressed_size,
            "compressed ({compressed_size}) should be smaller than uncompressed ({uncompressed_size}) for {key}"
        );

        // Parseable trace events.
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
        .filter(|e| e.metrics.get("Success").is_some_and(|v| *v == true))
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
}

/// When S3 hangs permanently (put_object never returns), graceful_shutdown
/// must still complete within its timeout instead of blocking forever.
#[test]
fn graceful_shutdown_completes_when_s3_hangs() {
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

    let mut builder = tokio::runtime::Builder::new_multi_thread();
    builder.worker_threads(2).enable_all();

    let (runtime, guard) = TracedRuntime::builder()
        .with_trace_path(&trace_path)
        .with_s3_uploader(s3_config.clone())
        .with_s3_client(client.clone())
        .with_worker_poll_interval(std::time::Duration::from_millis(50))
        .build_and_start(builder, writer)
        .unwrap();

    // Generate trace data on the TracedRuntime, then let the worker pick it up.
    let handle = guard.handle();
    runtime.block_on(async {
        for _ in 0..50 {
            handle.spawn(async { tokio::task::yield_now().await });
        }
        tokio::time::sleep(std::time::Duration::from_secs(1)).await;
    });

    drop(runtime);

    // graceful_shutdown should complete within the timeout, not hang forever.
    let shutdown_timeout = std::time::Duration::from_secs(3);
    let test_deadline = std::time::Duration::from_secs(10);

    let (tx, rx) = std::sync::mpsc::channel();
    let t = std::thread::spawn(move || {
        let result = guard.graceful_shutdown(shutdown_timeout);
        let _ = tx.send(result);
    });

    let result = rx.recv_timeout(test_deadline);

    // If the test-level timeout fires, graceful_shutdown hung — that's the bug.
    assert!(
        result.is_ok(),
        "graceful_shutdown hung beyond {test_deadline:?} — it did not respect its own {shutdown_timeout:?} timeout"
    );

    let _ = result.unwrap();
    let _ = t.join();
}

/// Stress test with injected S3 failures.
///
/// Same as `stress_test_all_segments_uploaded_and_valid` but every 3rd
/// S3 operation returns InternalError. The worker must handle failures
/// gracefully and still upload what it can.
#[test]
fn stress_test_with_s3_failures() {
    let s3_root = tempfile::tempdir().unwrap();
    let trace_dir = tempfile::tempdir().unwrap();
    let trace_path = trace_dir.path().join("trace.bin");

    std::fs::create_dir(s3_root.path().join("flaky-bucket")).unwrap();
    let client = fake_s3_client_flaky(s3_root.path(), "us-east-1", 3);

    let segment_size = 64 * 1024;
    let total_size = 2 * 1024 * 1024;
    let writer = RotatingWriter::new(&trace_path, segment_size, total_size).unwrap();

    let s3_config = S3Config::builder()
        .bucket("flaky-bucket")
        .prefix("traces")
        .service_name("flaky-svc")
        .instance_path("test-host")
        .boot_id("flaky-boot")
        .region("us-east-1")
        .build();

    let mut builder = tokio::runtime::Builder::new_multi_thread();
    builder.worker_threads(4).enable_all();

    let (runtime, guard) = TracedRuntime::builder()
        .with_task_tracking(true)
        .with_trace_path(&trace_path)
        .with_s3_uploader(s3_config.clone())
        .with_s3_client(client.clone())
        .with_worker_poll_interval(std::time::Duration::from_millis(50))
        .build_and_start(builder, writer)
        .unwrap();

    let handle = guard.handle();

    runtime.block_on(async {
        let deadline = tokio::time::Instant::now() + std::time::Duration::from_secs(1);
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
    });

    drop(runtime);
    guard
        .graceful_shutdown(std::time::Duration::from_secs(10))
        .expect("graceful shutdown");

    // Verify some objects landed in S3 despite failures.
    let verify_client = fake_s3_client(s3_root.path());
    let list_rt = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .unwrap();
    let object_count = list_rt.block_on(async {
        let resp = verify_client
            .list_objects_v2()
            .bucket("flaky-bucket")
            .prefix("traces/")
            .send()
            .await
            .unwrap();
        resp.key_count.unwrap_or(0)
    });

    assert!(
        object_count > 0,
        "expected some successful uploads despite flaky S3"
    );
}

/// When S3 is permanently returning 500s, every segment attempt should
/// produce a failure metric entry.
#[test]
fn permanently_broken_s3_produces_failure_metrics() {
    let s3_root = tempfile::tempdir().unwrap();
    let trace_dir = tempfile::tempdir().unwrap();
    let trace_path = trace_dir.path().join("trace.bin");

    std::fs::create_dir_all(s3_root.path().join("broken-bucket")).unwrap();
    let client = fake_s3_client_always_failing(s3_root.path());

    let writer = RotatingWriter::new(&trace_path, 512, 50 * 1024).unwrap();

    let s3_config = S3Config::builder()
        .bucket("broken-bucket")
        .prefix("traces")
        .service_name("test-svc")
        .instance_path("test-host")
        .boot_id("test-boot")
        .region("us-east-1")
        .build();

    let metrique_writer::test_util::TestEntrySink {
        inspector,
        sink: metrics_sink,
    } = metrique_writer::test_util::test_entry_sink();

    let mut builder = tokio::runtime::Builder::new_multi_thread();
    builder.worker_threads(2).enable_all();

    let (runtime, guard) = TracedRuntime::builder()
        .with_trace_path(&trace_path)
        .with_s3_uploader(s3_config.clone())
        .with_s3_client(client.clone())
        .with_worker_poll_interval(std::time::Duration::from_millis(50))
        .with_worker_metrics_sink(metrics_sink)
        .build_and_start(builder, writer)
        .unwrap();

    // Generate enough events to seal at least one segment, then shut down.
    runtime.block_on(async {
        for _ in 0..50 {
            tokio::spawn(async { tokio::task::yield_now().await });
        }
        tokio::time::sleep(std::time::Duration::from_secs(1)).await;
    });

    drop(runtime);
    guard
        .graceful_shutdown(std::time::Duration::from_secs(5))
        .expect("graceful shutdown");

    let entries = inspector.entries();
    // Filter to pipeline metrics only (FlushMetrics entries don't have Failure/Success keys).
    let pipeline_entries: Vec<_> = entries
        .iter()
        .filter(|e| e.metrics.contains_key("Failure") || e.metrics.contains_key("Success"))
        .collect();
    assert!(
        !pipeline_entries.is_empty(),
        "expected at least one pipeline metric entry"
    );

    let failures = pipeline_entries
        .iter()
        .filter(|e| e.metrics["Failure"] == true)
        .count();
    assert_eq!(
        failures,
        pipeline_entries.len(),
        "all {} entries should be failures when S3 is permanently broken, but {} succeeded",
        pipeline_entries.len(),
        pipeline_entries.len() - failures,
    );
}
