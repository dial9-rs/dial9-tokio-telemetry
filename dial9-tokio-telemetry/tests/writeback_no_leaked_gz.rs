//! End-to-end test: processed `.bin.gz` files must not leak on disk.
//!
//! The background worker writes compressed segments as `.bin.gz` and deletes
//! the original `.bin`.  When the writer evicts old segments, it must also
//! clean up the renamed `.bin.gz` variants.  A leak here means unbounded disk
//! growth in production.
#![cfg(feature = "cpu-profiling")]

use dial9_tokio_telemetry::telemetry::cpu_profile::CpuProfilingConfig;
use dial9_tokio_telemetry::telemetry::{RotatingWriter, TracedRuntime};
use std::time::Duration;

/// Produce enough trace data to trigger multiple rotations and evictions,
/// then verify that:
/// 1. No unprocessed `.bin` files remain (worker processed everything).
/// 2. The number of `.bin.gz` files on disk respects the eviction budget
///    (no leaked processed segments).
#[test]
fn eviction_cleans_up_processed_gz_segments() {
    let _ = tracing_subscriber::fmt()
        .with_env_filter(tracing_subscriber::EnvFilter::from_default_env())
        .try_init();

    let trace_dir = tempfile::tempdir().unwrap();
    let trace_path = trace_dir.path().join("trace.bin");

    // Small file/total budget to force frequent rotation and eviction.
    // Each segment holds roughly one flush cycle worth of events.
    let max_file_size = 4 * 1024; // 4 KiB per segment
    let max_number_files = 4;
    let max_total_size = max_number_files * max_file_size; // 16 KiB total ⇒ ~4 segments before eviction

    let writer = RotatingWriter::new(&trace_path, max_file_size, max_total_size).unwrap();

    let mut builder = tokio::runtime::Builder::new_multi_thread();
    builder.worker_threads(2).enable_all();

    let (runtime, guard) = TracedRuntime::builder()
        .with_cpu_profiling(CpuProfilingConfig::default())
        .with_trace_path(&trace_path)
        .with_worker_poll_interval(Duration::from_millis(50))
        .build_and_start(builder, writer)
        .unwrap();

    // Generate enough work to produce many sealed segments, exceeding the
    // total budget so eviction must kick in.
    runtime.block_on(async {
        for _ in 0..30 {
            let mut handles = Vec::new();
            for _ in 0..20 {
                handles.push(tokio::spawn(async {
                    for _ in 0..50 {
                        tokio::task::yield_now().await;
                    }
                }));
            }
            for h in handles {
                let _ = h.await;
            }
            // Give the worker time to process sealed segments between bursts.
            tokio::time::sleep(Duration::from_millis(80)).await;
        }
    });

    drop(runtime);
    guard
        .graceful_shutdown(Duration::from_secs(10))
        .expect("graceful shutdown");

    // Collect all trace-related files in the directory.
    let mut bin_files = Vec::new();
    let mut gz_files = Vec::new();
    for entry in std::fs::read_dir(trace_dir.path()).unwrap() {
        let path = entry.unwrap().path();
        let name = path.file_name().unwrap().to_string_lossy().to_string();
        if name.ends_with(".bin") {
            bin_files.push(path);
        } else if name.ends_with(".bin.gz") {
            gz_files.push(path);
        }
    }

    // After graceful_shutdown the worker has processed all sealed segments,
    // so no raw `.bin` files should remain.
    assert!(
        bin_files.is_empty(),
        "expected no unprocessed .bin files after graceful shutdown, found: {bin_files:?}"
    );

    // The writer's eviction budget is ~4 segments (max_total_size / max_file_size).
    assert!(
        gz_files.len() <= max_number_files as usize,
        "expected less or equal than max number of gz files {}",
        gz_files.len()
    );

    // Compute total size of .gz files on disk.
    let total_gz_bytes: u64 = gz_files
        .iter()
        .map(|p| std::fs::metadata(p).unwrap().len())
        .sum();

    // The total compressed size on disk must be smaller or equal to the max total size
    assert!(
        total_gz_bytes <= max_total_size,
        "total .bin.gz size on disk ({total_gz_bytes} bytes) exceeds the configured \
         budget ({max_total_size} bytes) — processed segments are leaking. \
         Files: {gz_files:?}"
    );
}
