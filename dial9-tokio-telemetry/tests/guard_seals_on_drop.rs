use dial9_tokio_telemetry::telemetry::{RotatingWriter, TracedRuntime};
use std::time::Duration;

/// After TelemetryGuard is dropped, all trace files should be sealed (.bin),
/// with no .active files remaining. This is the contract the worker depends on.
#[test]
fn guard_drop_produces_sealed_bin_files() {
    let dir = tempfile::tempdir().unwrap();
    let trace_path = dir.path().join("trace.bin");

    let mut builder = tokio::runtime::Builder::new_multi_thread();
    builder.worker_threads(2).enable_all();

    let writer = RotatingWriter::new(&trace_path, 1024, 1024 * 1024)
        .unwrap();
    let (runtime, guard) = TracedRuntime::build_and_start(builder, writer).unwrap();

    runtime.block_on(async {
        for _ in 0..100 {
            tokio::spawn(async { tokio::task::yield_now().await });
        }
        tokio::time::sleep(Duration::from_millis(300)).await;
    });

    drop(runtime);
    drop(guard);

    let entries: Vec<_> = std::fs::read_dir(dir.path())
        .unwrap()
        .filter_map(|e| e.ok())
        .collect();

    let bin_files: Vec<_> = entries
        .iter()
        .filter(|e| e.path().extension().is_some_and(|ext| ext == "bin"))
        .collect();
    let active_files: Vec<_> = entries
        .iter()
        .filter(|e| e.path().extension().is_some_and(|ext| ext == "active"))
        .collect();

    assert!(!bin_files.is_empty(), "should have at least one .bin file");
    assert!(
        active_files.is_empty(),
        "no .active files should remain after guard drop, found: {:?}",
        active_files.iter().map(|e| e.path()).collect::<Vec<_>>()
    );
}
