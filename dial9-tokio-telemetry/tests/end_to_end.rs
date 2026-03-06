mod common;
mod validation;

use dial9_tokio_telemetry::telemetry::events::TelemetryEvent;
use dial9_tokio_telemetry::telemetry::{
    SimpleBinaryWriter, TraceReader, TracedRuntime, analyze_trace,
};
use std::time::Duration;

/// Run a known workload under TracedRuntime, read the trace back, and verify
/// the analysis is consistent with both the workload parameters and tokio's
/// RuntimeMetrics.
#[test]
fn end_to_end_trace_matches_workload_and_metrics() {
    let dir = tempfile::tempdir().unwrap();
    let trace_path = dir.path().join("trace.bin");

    let num_workers = 4;
    let total_tasks: usize = 2000;

    let mut builder = tokio::runtime::Builder::new_multi_thread();
    builder.worker_threads(num_workers).enable_all();

    let writer = SimpleBinaryWriter::new(&trace_path).unwrap();
    let (runtime, guard) = TracedRuntime::build_and_start(builder, Box::new(writer)).unwrap();

    // Run workload, then snapshot tokio metrics.
    let tokio_metrics = runtime.block_on(async {
        let mut handles = Vec::new();
        for _ in 0..total_tasks {
            handles.push(tokio::spawn(async {
                for _ in 0..10 {
                    tokio::task::yield_now().await;
                }
                tokio::time::sleep(Duration::from_millis(2)).await;
            }));
        }
        for h in handles {
            h.await.unwrap();
        }

        // Wait for flush cycle to drain thread-local buffers.
        tokio::time::sleep(Duration::from_millis(600)).await;

        // Grab metrics handle while still inside the runtime.
        tokio::runtime::Handle::current().metrics()
    });

    // Drop runtime first — workers park, flushing thread-local buffers
    // while telemetry is still enabled.
    drop(runtime);
    // Drop guard — stops flush thread and does final collector drain.
    drop(guard);

    // --- Read the trace back ---
    let mut reader = TraceReader::new(trace_path.to_str().unwrap()).unwrap();
    reader.read_header().unwrap();
    let events = reader.read_all().unwrap();
    let analysis = analyze_trace(&events);

    validation::validate_trace_matches_metrics(&analysis, &events, &tokio_metrics);
}

/// Regression test: TaskSpawn events emitted on the main thread (inside block_on)
/// must appear in the trace. Before the fix, the main thread's buffer was never
/// flushed (no WorkerPark fires on main), so all these events were silently dropped.
#[test]
fn task_spawn_events_from_main_thread_are_captured() {
    let (writer, events) = common::CapturingWriter::new();

    const N: usize = 10;

    let mut builder = tokio::runtime::Builder::new_multi_thread();
    builder.worker_threads(2).enable_all();

    let (runtime, guard) = TracedRuntime::builder()
        .with_task_tracking(true)
        .build_and_start(builder, Box::new(writer))
        .unwrap();

    // All tokio::spawn calls here fire on the main (block_on) thread,
    // so their TaskSpawn events land in the main thread's buffer.
    runtime.block_on(async {
        let mut handles = Vec::new();
        for _ in 0..N {
            handles.push(tokio::spawn(async {}));
        }
        for h in handles {
            h.await.unwrap();
        }
    });

    drop(runtime);
    drop(guard);

    let events = events.lock().unwrap();
    let task_spawn_count = events
        .iter()
        .filter(|e| matches!(e, TelemetryEvent::TaskSpawn { .. }))
        .count();

    assert_eq!(
        task_spawn_count, N,
        "expected {N} TaskSpawn events from main thread, got {task_spawn_count}"
    );
}

#[test]
fn task_terminate_events_are_captured() {
    let (writer, events) = common::CapturingWriter::new();

    const N: usize = 10;

    let mut builder = tokio::runtime::Builder::new_multi_thread();
    builder.worker_threads(2).enable_all();

    let (runtime, guard) = TracedRuntime::builder()
        .with_task_tracking(true)
        .build_and_start(builder, Box::new(writer))
        .unwrap();

    runtime.block_on(async {
        let mut handles = Vec::new();
        for _ in 0..N {
            handles.push(tokio::spawn(async {}));
        }
        for h in handles {
            h.await.unwrap();
        }
    });

    drop(runtime);
    drop(guard);

    let events = events.lock().unwrap();
    let terminate_count = events
        .iter()
        .filter(|e| matches!(e, TelemetryEvent::TaskTerminate { .. }))
        .count();

    // Tokio treats worker threads as tasks, so we get N + num_workers terminate
    // events. This is arguably a Tokio bug — workers shouldn't emit task lifecycle
    // events — but we assert the exact count to catch regressions.
    let num_workers = 2;
    let expected = N + num_workers;
    assert_eq!(
        terminate_count, expected,
        "expected {expected} TaskTerminate events (N={N} tasks + {num_workers} workers), got {terminate_count}"
    );
}
