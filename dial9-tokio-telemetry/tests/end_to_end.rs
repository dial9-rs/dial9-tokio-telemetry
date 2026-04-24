mod common;
mod validation;

use dial9_tokio_telemetry::analysis_unstable::{TraceReader, analyze_trace};
use dial9_tokio_telemetry::telemetry::{RotatingWriter, TelemetryEvent, TracedRuntime};
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

    let writer = RotatingWriter::single_file(&trace_path).unwrap();
    let (runtime, guard) = TracedRuntime::build_and_start(builder, writer).unwrap();

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
    let sealed_path = dir.path().join("trace.0.bin");
    let reader = TraceReader::new(sealed_path.to_str().unwrap()).unwrap();
    let events = &reader.runtime_events;
    let analysis = analyze_trace(events);

    validation::validate_trace_matches_metrics(&analysis, events, &tokio_metrics);
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
        .build_and_start(builder, writer)
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
        .build_and_start(builder, writer)
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

    // Tokio may emit TaskTerminate for internal tasks (e.g. worker threads),
    // so we assert at least N terminate events rather than an exact count.
    assert!(
        terminate_count >= N,
        "expected at least {N} TaskTerminate events, got {terminate_count}"
    );
}

#[test]
fn custom_event_appears_in_trace() {
    use dial9_trace_format::TraceEvent as TraceEventDerive;

    #[derive(TraceEventDerive)]
    struct MyCustomEvent {
        #[traceevent(timestamp)]
        timestamp_ns: u64,
        request_count: u32,
    }

    let dir = tempfile::tempdir().unwrap();
    let trace_path = dir.path().join("trace.bin");

    let mut builder = tokio::runtime::Builder::new_multi_thread();
    builder.worker_threads(2).enable_all();

    let writer = RotatingWriter::single_file(&trace_path).unwrap();
    let (runtime, guard) = TracedRuntime::build_and_start(builder, writer).unwrap();

    let handle = guard.handle();
    runtime.block_on(async {
        for i in 0..5 {
            dial9_tokio_telemetry::telemetry::record_event(
                MyCustomEvent {
                    timestamp_ns: dial9_tokio_telemetry::telemetry::clock_monotonic_ns(),
                    request_count: i,
                },
                &handle,
            );
        }
        // Wait for flush cycle
        tokio::time::sleep(Duration::from_millis(200)).await;
    });

    drop(runtime);
    drop(guard);

    // Decode at the trace-format level to find our custom event
    let sealed_path = dir.path().join("trace.0.bin");
    let data = std::fs::read(&sealed_path).unwrap();
    let mut decoder = dial9_trace_format::decoder::Decoder::new(&data).unwrap();
    let mut custom_count = 0u32;
    decoder
        .for_each_event(|ev| {
            if ev.name == "MyCustomEvent" {
                custom_count += 1;
            }
        })
        .unwrap();
    assert_eq!(custom_count, 5, "expected 5 MyCustomEvent events in trace");
}

#[test]
fn spawn_audit_detects_uninstrumented_spawns() {
    let dir = tempfile::tempdir().unwrap();
    let trace_path = dir.path().join("trace.bin");

    const RAW: usize = 5;
    const INSTRUMENTED: usize = 3;

    let mut builder = tokio::runtime::Builder::new_multi_thread();
    builder.worker_threads(2).enable_all();

    let writer = RotatingWriter::single_file(&trace_path).unwrap();
    let (runtime, guard) = TracedRuntime::builder()
        .with_task_tracking(true)
        .build_and_start(builder, writer)
        .unwrap();

    let handle = guard.handle();

    runtime.block_on(async {
        let mut joins = Vec::new();

        // These go through TelemetryHandle::spawn, should NOT be flagged.
        for _ in 0..INSTRUMENTED {
            joins.push(handle.spawn(async {}));
        }

        // These are raw tokio::spawn, SHOULD be flagged, all at the same line.
        for _ in 0..RAW {
            joins.push(tokio::spawn(async {}));
        }

        for j in joins {
            j.await.unwrap();
        }

        // Wait for flush cycle to drain thread-local buffers.
        tokio::time::sleep(Duration::from_millis(200)).await;
    });

    drop(runtime);
    drop(guard);

    // Read the trace back from disk and check the instrumented flag.
    let sealed_path = dir.path().join("trace.0.bin");
    let reader = TraceReader::new(sealed_path.to_str().unwrap()).unwrap();
    let events = &reader.all_events;

    let instrumented_count = events
        .iter()
        .filter(|e| {
            matches!(
                e,
                TelemetryEvent::TaskSpawn {
                    instrumented: true,
                    ..
                }
            )
        })
        .count();
    let uninstrumented_count = events
        .iter()
        .filter(|e| {
            matches!(
                e,
                TelemetryEvent::TaskSpawn {
                    instrumented: false,
                    ..
                }
            )
        })
        .count();

    assert_eq!(
        instrumented_count, INSTRUMENTED,
        "expected {INSTRUMENTED} instrumented spawns, got {instrumented_count}"
    );
    assert_eq!(
        uninstrumented_count, RAW,
        "expected {RAW} uninstrumented spawns, got {uninstrumented_count}"
    );

    // Verify spawn locations resolve and point to this test file.
    for event in events {
        if let TelemetryEvent::TaskSpawn {
            spawn_loc,
            instrumented: false,
            ..
        } = event
        {
            let loc = reader
                .spawn_locations
                .get(spawn_loc)
                .expect("uninstrumented spawn_loc should resolve");
            assert!(
                loc.contains("end_to_end.rs"),
                "uninstrumented spawn should point to this test file, got: {loc}"
            );
        }
    }
}
