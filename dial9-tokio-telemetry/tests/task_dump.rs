mod common;

use dial9_tokio_telemetry::telemetry::{TaskDumpConfig, TaskId, TelemetryEvent, TracedRuntime};
use std::sync::{Arc, Mutex};
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

/// Per-task enable_task_dumps() must not leak to other tasks on the same OS thread.
/// Uses current_thread runtime so both tasks share one thread.
#[test]
fn per_task_override_does_not_leak_across_tasks() {
    let (writer, events) = common::CapturingWriter::new();

    let mut builder = tokio::runtime::Builder::new_current_thread();
    builder.enable_all();
    // Task dumps globally DISABLED — only the per-task override should fire
    let (runtime, guard) = TracedRuntime::builder()
        .with_task_tracking(true)
        .build_and_start(builder, writer)
        .unwrap();

    let handle = guard.handle();

    let opted_in_id = Arc::new(Mutex::new(TaskId::default()));
    let opted_out_id = Arc::new(Mutex::new(TaskId::default()));
    let id1 = opted_in_id.clone();
    let id2 = opted_out_id.clone();

    runtime.block_on(async {
        // Task A: opts in to task dumps
        let a = handle.spawn(async move {
            *id1.lock().unwrap() = tokio::task::try_id().map(TaskId::from).unwrap_or_default();
            dial9_tokio_telemetry::enable_task_dumps();
            // Sleep long enough to exceed the 10ms default threshold
            tokio::time::sleep(Duration::from_millis(50)).await;
        });

        // Task B: does NOT opt in — must produce zero TaskDump events
        let b = handle.spawn(async move {
            *id2.lock().unwrap() = tokio::task::try_id().map(TaskId::from).unwrap_or_default();
            tokio::time::sleep(Duration::from_millis(50)).await;
        });

        a.await.unwrap();
        b.await.unwrap();
    });

    drop(runtime);
    drop(guard);

    let events = events.lock().unwrap();
    let opted_in = *opted_in_id.lock().unwrap();
    let opted_out = *opted_out_id.lock().unwrap();

    let dumps_for_opted_in = events
        .iter()
        .filter(|e| matches!(e, TelemetryEvent::TaskDump { task_id, .. } if *task_id == opted_in))
        .count();
    let dumps_for_opted_out = events
        .iter()
        .filter(|e| matches!(e, TelemetryEvent::TaskDump { task_id, .. } if *task_id == opted_out))
        .count();

    assert!(
        dumps_for_opted_in > 0,
        "task that called enable_task_dumps() should have TaskDump events"
    );
    assert_eq!(
        dumps_for_opted_out, 0,
        "task that did NOT call enable_task_dumps() must have zero TaskDump events, \
         but got {dumps_for_opted_out} — the override leaked across tasks"
    );
}
