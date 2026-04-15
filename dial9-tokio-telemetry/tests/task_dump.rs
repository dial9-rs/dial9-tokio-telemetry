mod common;

use dial9_tokio_telemetry::telemetry::{TaskDumpConfig, TelemetryEvent, TracedRuntime};
use std::time::Duration;

/// Task dumps are emitted for tasks that sleep longer than the idle threshold.
#[test]
fn task_dump_emitted_for_long_sleep() {
    let (writer, events) = common::CapturingWriter::new();

    let mut builder = tokio::runtime::Builder::new_current_thread();
    builder.enable_all();

    let (runtime, guard) = TracedRuntime::builder()
        .with_task_tracking(true)
        .with_task_dumps(TaskDumpConfig::enabled())
        .build_and_start(builder, writer)
        .unwrap();

    let handle = guard.handle();

    runtime.block_on(async {
        let join = handle.spawn(async {
            // 50ms sleep — well above 10ms threshold
            tokio::time::sleep(Duration::from_millis(50)).await;
        });
        join.await.unwrap();
    });

    drop(runtime);
    drop(guard);

    let events = events.lock().unwrap();
    let task_dumps: Vec<_> = events
        .iter()
        .filter(|e| matches!(e, TelemetryEvent::TaskDump { .. }))
        .collect();

    assert!(
        !task_dumps.is_empty(),
        "expected TaskDump events for a 50ms sleep with 10ms threshold, got none; all events: {:?}",
        events
            .iter()
            .map(std::mem::discriminant)
            .collect::<Vec<_>>()
    );

    // Verify callchains are non-empty
    for dump in &task_dumps {
        if let TelemetryEvent::TaskDump { callchain, .. } = dump {
            assert!(
                !callchain.is_empty(),
                "TaskDump callchain should not be empty"
            );
        }
    }
}

/// Task dumps are NOT emitted for tasks that sleep less than the idle threshold.
#[test]
fn no_task_dump_for_short_sleep() {
    let (writer, events) = common::CapturingWriter::new();

    let mut builder = tokio::runtime::Builder::new_current_thread();
    builder.enable_all();

    let (runtime, guard) = TracedRuntime::builder()
        .with_task_tracking(true)
        .with_task_dumps(TaskDumpConfig::enabled())
        .build_and_start(builder, writer)
        .unwrap();

    let handle = guard.handle();

    runtime.block_on(async {
        let join = handle.spawn(async {
            // 1ms sleep — below 10ms threshold
            tokio::time::sleep(Duration::from_millis(1)).await;
        });
        join.await.unwrap();
    });

    drop(runtime);
    drop(guard);

    let events = events.lock().unwrap();
    let task_dump_count = events
        .iter()
        .filter(|e| matches!(e, TelemetryEvent::TaskDump { .. }))
        .count();

    assert_eq!(
        task_dump_count, 0,
        "expected no TaskDump events for a 1ms sleep with 10ms threshold"
    );
}
