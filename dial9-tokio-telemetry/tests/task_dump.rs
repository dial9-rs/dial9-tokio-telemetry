#![cfg(feature = "taskdump")]

mod common;

use dial9_tokio_telemetry::telemetry::{TaskDumpConfig, TelemetryEvent, TracedRuntime};
use std::time::Duration;

/// A task that stays idle longer than the threshold between polls should
/// produce at least one `TaskDump` event.
#[test]
fn task_dump_emitted_for_long_sleep() {
    let (writer, events) = common::CapturingWriter::new();

    let mut builder = tokio::runtime::Builder::new_current_thread();
    builder.enable_all();
    let (runtime, guard) = TracedRuntime::builder()
        .with_task_tracking(true)
        .with_task_dumps(TaskDumpConfig::default())
        .build_and_start(builder, writer)
        .unwrap();

    let handle = guard.handle();
    runtime.block_on(async {
        let join = handle.spawn(async {
            // Well above the 10ms default threshold.
            tokio::time::sleep(Duration::from_millis(50)).await;
        });
        join.await.unwrap();
    });

    drop(runtime);
    drop(guard);

    let events = events.lock().unwrap();
    let dumps: Vec<_> = events
        .iter()
        .filter(|e| matches!(e, TelemetryEvent::TaskDump { .. }))
        .collect();

    assert!(
        !dumps.is_empty(),
        "expected TaskDump events; got: {:?}",
        events
            .iter()
            .map(std::mem::discriminant)
            .collect::<Vec<_>>()
    );
    for dump in &dumps {
        if let TelemetryEvent::TaskDump { callchain, .. } = dump {
            assert!(!callchain.is_empty(), "callchain must be non-empty");
        }
    }
}

/// A task whose idles are all below threshold should produce zero dumps.
#[test]
fn no_task_dump_for_short_sleep() {
    let (writer, events) = common::CapturingWriter::new();

    let mut builder = tokio::runtime::Builder::new_current_thread();
    builder.enable_all();
    let (runtime, guard) = TracedRuntime::builder()
        .with_task_tracking(true)
        .with_task_dumps(
            TaskDumpConfig::builder()
                .idle_threshold(Duration::from_secs(1))
                .build(),
        )
        .build_and_start(builder, writer)
        .unwrap();

    let handle = guard.handle();
    runtime.block_on(async {
        let join = handle.spawn(async {
            tokio::time::sleep(Duration::from_millis(1)).await;
        });
        join.await.unwrap();
    });

    drop(runtime);
    guard
        .graceful_shutdown(Duration::from_secs(1))
        .expect("clean shutdown");

    let events = events.lock().unwrap();
    let dump_count = events
        .iter()
        .filter(|e| matches!(e, TelemetryEvent::TaskDump { .. }))
        .count();
    assert_eq!(dump_count, 0, "expected no TaskDump events");
}

/// Wrapping with `TaskDumped` must not produce duplicate wake or poll events:
/// the same workload with and without dumps enabled must produce the same
/// number of `PollStart`/`PollEnd`/`WakeEvent` entries.
#[test]
fn task_dump_does_not_produce_extra_events() {
    fn run(enable: bool) -> (usize, usize, usize) {
        let (writer, events) = common::CapturingWriter::new();

        let mut builder = tokio::runtime::Builder::new_current_thread();
        builder.enable_all();
        let mut tb = TracedRuntime::builder().with_task_tracking(true);
        if enable {
            tb = tb.with_task_dumps(TaskDumpConfig::default());
        }
        let (runtime, guard) = tb.build_and_start(builder, writer).unwrap();

        let handle = guard.handle();
        runtime.block_on(async {
            let join = handle.spawn(async {
                tokio::task::yield_now().await;
                tokio::task::yield_now().await;
                tokio::task::yield_now().await;
            });
            join.await.unwrap();
        });
        drop(runtime);
        guard
            .graceful_shutdown(Duration::from_secs(1))
            .expect("clean shutdown");

        let events = events.lock().unwrap();
        let mut starts = 0usize;
        let mut ends = 0usize;
        let mut wakes = 0usize;
        for e in events.iter() {
            match e {
                TelemetryEvent::PollStart { .. } => starts += 1,
                TelemetryEvent::PollEnd { .. } => ends += 1,
                TelemetryEvent::WakeEvent { .. } => wakes += 1,
                _ => {}
            }
        }
        (starts, ends, wakes)
    }

    let baseline = run(false);
    let with_dumps = run(true);
    assert_eq!(
        baseline, with_dumps,
        "enabling task dumps changed PollStart/PollEnd/WakeEvent counts: {baseline:?} vs {with_dumps:?}"
    );
}
