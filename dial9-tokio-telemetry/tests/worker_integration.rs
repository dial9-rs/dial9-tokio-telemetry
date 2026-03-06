//! Integration test: in-process worker starts and stops cleanly.
#![cfg(feature = "worker-s3")]

use dial9_tokio_telemetry::telemetry::{RotatingWriter, TracedRuntime};
use dial9_tokio_telemetry::worker::WorkerConfig;

#[test]
fn worker_thread_starts_and_stops_cleanly() {
    let trace_dir = tempfile::tempdir().unwrap();
    let trace_path = trace_dir.path().join("trace.bin");

    let writer = RotatingWriter::new(&trace_path, 1024, 10 * 1024).unwrap();

    let worker_config = WorkerConfig::builder()
        .trace_path(&trace_path)
        .poll_interval(std::time::Duration::from_millis(50))
        .build()
        .unwrap();

    let mut builder = tokio::runtime::Builder::new_multi_thread();
    builder.worker_threads(1).enable_all();

    let (runtime, guard) = TracedRuntime::builder()
        .in_process_worker(worker_config)
        .build(builder, writer)
        .unwrap();

    // Run a trivial workload
    runtime.block_on(async {
        tokio::time::sleep(std::time::Duration::from_millis(100)).await;
    });

    // Drop guard — should stop flush thread, seal, stop worker, all cleanly
    drop(guard);
    drop(runtime);
}
