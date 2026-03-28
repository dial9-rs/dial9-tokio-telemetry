//! After graceful_shutdown, worker-processed segments must be clean gzip with
//! no trailing data. Previously, TelemetryRecorder::Drop flushed queued events
//! through a stale file descriptor after the worker had already compressed and
//! rewritten the segment, appending trailing garbage.
#![cfg(feature = "cpu-profiling")]

use dial9_tokio_telemetry::telemetry::{CpuProfilingConfig, RotatingWriter, TracedRuntime};
use flate2::read::GzDecoder;
use std::io::Read;

#[test]
fn graceful_shutdown_produces_clean_gzip_segments() {
    let trace_dir = tempfile::tempdir().unwrap();
    let trace_path = trace_dir.path().join("trace.bin");

    let writer = RotatingWriter::new(&trace_path, 512 * 1024, 10 * 1024 * 1024).unwrap();

    let mut builder = tokio::runtime::Builder::new_multi_thread();
    builder.worker_threads(2).enable_all();

    let (runtime, guard) = TracedRuntime::builder()
        .with_cpu_profiling(CpuProfilingConfig::default())
        .with_trace_path(&trace_path)
        .with_worker_poll_interval(std::time::Duration::from_millis(50))
        .build_and_start(builder, writer)
        .unwrap();

    runtime.block_on(async {
        // Spawn enough work to fill thread-local buffers. The bug requires
        // unflushed events in thread-local buffers at graceful_shutdown time,
        // which then get written through a stale fd in Drop.
        for _ in 0..20 {
            let mut handles = Vec::new();
            for _ in 0..10 {
                handles.push(tokio::spawn(async {
                    for _ in 0..100 {
                        tokio::task::yield_now().await;
                    }
                }));
            }
            for h in handles {
                let _ = h.await;
            }
        }
    });

    drop(runtime);
    guard
        .graceful_shutdown(std::time::Duration::from_secs(10))
        .expect("graceful shutdown");

    let mut gzip_files = 0;
    for entry in std::fs::read_dir(trace_dir.path()).unwrap() {
        let path = entry.unwrap().path();
        if path.extension().is_none_or(|ext| ext != "bin") {
            continue;
        }
        let raw = std::fs::read(&path).unwrap();
        if raw.len() < 2 || raw[0] != 0x1f || raw[1] != 0x8b {
            continue;
        }
        gzip_files += 1;

        // Decompress and check that the gzip stream consumes the entire file.
        let mut decoder = GzDecoder::new(&raw[..]);
        let mut decompressed = Vec::new();
        decoder
            .read_to_end(&mut decompressed)
            .expect("gzip decompression should succeed");

        let consumed = raw.len() as u64 - decoder.into_inner().len() as u64;

        // The gzip stream must consume the entire file (no trailing garbage).
        assert_eq!(
            consumed,
            raw.len() as u64,
            "gzip segment {} has {} trailing bytes",
            path.display(),
            raw.len() as u64 - consumed
        );
    }

    assert!(
        gzip_files > 0,
        "expected at least one gzip-compressed segment"
    );
}
