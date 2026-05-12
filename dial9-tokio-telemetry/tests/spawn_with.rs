//! Integration tests for the custom-spawn tracing API:
//! - [`TelemetryHandle::spawn_with`]
//! - [`RuntimeTelemetryHandle::spawn_with`]
//!
//! `spawn_with(fut, |f| spawn_fn(f))` is the way to spawn an instrumented
//! future through APIs other than `TelemetryHandle::spawn` (e.g.
//! `tokio::task::JoinSet::spawn`, `JoinSet::spawn_on`, etc.). It must:
//!   - emit `WakeEvent`s for the polled future, and
//!   - mark the resulting `TaskSpawn` as `instrumented = true`,
//!   - while preserving the user's call site as `spawn_loc`.

mod common;

use dial9_tokio_telemetry::analysis_unstable::TraceReader;
use dial9_tokio_telemetry::telemetry::{
    RotatingWriter, TaskId, TelemetryEvent, TelemetryGuard, TraceWriter, TracedRuntime,
};
use std::sync::{Arc, Mutex};
use std::time::Duration;
use tokio::runtime::Runtime;
use tokio::task::JoinSet;

/// Standard 2-worker multi_thread runtime with task tracking enabled.
fn build_traced_runtime<W: TraceWriter + 'static>(writer: W) -> (Runtime, TelemetryGuard) {
    let mut builder = tokio::runtime::Builder::new_multi_thread();
    builder.worker_threads(2).enable_all();
    TracedRuntime::builder()
        .with_task_tracking(true)
        .build_and_start(builder, writer)
        .unwrap()
}

/// `spawn_with(fut, |f| set.spawn(f))` produces `WakeEvent`s for the
/// spawned task — the same as `handle.spawn(fut)` would.
#[test]
fn spawn_with_joinset_emits_wake_events() {
    let (writer, events) = common::CapturingWriter::new();
    let (runtime, guard) = build_traced_runtime(writer);

    let handle = guard.handle();
    let spawned_id: Arc<Mutex<Option<TaskId>>> = Arc::new(Mutex::new(None));
    let id_w = spawned_id.clone();

    runtime.block_on(async move {
        let mut set: JoinSet<()> = JoinSet::new();
        // `yield_now().await` self-wakes through the active waker, which
        // here is our wake-tracking waker — so a `WakeEvent` fires without
        // depending on cross-task scheduling order.
        handle.spawn_with(
            async move {
                *id_w.lock().unwrap() = tokio::task::try_id().map(TaskId::from);
                tokio::task::yield_now().await;
            },
            |f| set.spawn(f),
        );
        while set.join_next().await.is_some() {}
        tokio::time::sleep(Duration::from_millis(200)).await;
    });

    drop(runtime);
    drop(guard);

    let events = events.lock().unwrap();
    let expected = spawned_id.lock().unwrap().expect("task id captured");
    let saw_wake = events.iter().any(|e| {
        matches!(e, TelemetryEvent::WakeEvent { woken_task_id, .. } if *woken_task_id == expected)
    });
    assert!(saw_wake, "expected WakeEvent for joinset task {expected:?}");
}

/// `spawn_with` flips the `TaskSpawn` `instrumented` flag for the spawn
/// performed inside the closure, AND because `JoinSet::spawn` is called
/// from that closure, its `#[track_caller]` resolves `spawn_loc` to the
/// closure call site (NOT the library).
#[test]
fn spawn_with_marks_taskspawn_and_preserves_caller() {
    let dir = tempfile::tempdir().unwrap();
    let trace_path = dir.path().join("trace.bin");
    let writer = RotatingWriter::single_file(&trace_path).unwrap();
    let (runtime, guard) = build_traced_runtime(writer);

    let handle = guard.handle();

    runtime.block_on(async move {
        let mut set: JoinSet<()> = JoinSet::new();

        // Inside `spawn_with`: marked instrumented, caller = this file.
        handle.spawn_with(async {}, |f| set.spawn(f));

        // Outside `spawn_with`: NOT instrumented.
        tokio::spawn(async {}).await.unwrap();

        while set.join_next().await.is_some() {}
        tokio::time::sleep(Duration::from_millis(200)).await;
    });

    drop(runtime);
    drop(guard);

    let sealed = dir.path().join("trace.0.bin");
    let reader = TraceReader::new(sealed.to_str().unwrap()).unwrap();

    let mut instrumented_user_loc = 0;
    let mut raw = 0;
    for event in &reader.all_events {
        if let TelemetryEvent::TaskSpawn {
            spawn_loc,
            instrumented,
            ..
        } = event
        {
            match instrumented {
                Some(true) => {
                    let loc = reader
                        .spawn_locations
                        .get(spawn_loc)
                        .expect("spawn_loc should resolve");
                    assert!(
                        loc.contains("spawn_with.rs"),
                        "instrumented spawn caller should resolve to the closure call site, got {loc}"
                    );
                    instrumented_user_loc += 1;
                }
                Some(false) => raw += 1,
                None => {}
            }
        }
    }
    assert_eq!(
        instrumented_user_loc, 1,
        "expected 1 instrumented TaskSpawn pointing to the closure call site"
    );
    assert!(raw >= 1, "expected at least 1 raw TaskSpawn, got {raw}");
}

/// `spawn_with` returns whatever the closure returns. Useful so callers
/// can keep the `JoinHandle` / `AbortHandle` / etc. produced by their
/// spawn function.
#[test]
fn spawn_with_returns_closure_value() {
    let (writer, _events) = common::CapturingWriter::new();
    let (runtime, guard) = build_traced_runtime(writer);

    let handle = guard.handle();

    runtime.block_on(async move {
        let join = handle.spawn_with(async { 42u32 }, tokio::spawn);
        let value = join.await.unwrap();
        assert_eq!(value, 42);
    });

    drop(runtime);
    drop(guard);
}

/// `RuntimeTelemetryHandle::spawn_with` composes with `JoinSet::spawn_on`
/// to target a specific runtime even when called from outside any
/// runtime context, while still emitting wake events and marking the
/// `TaskSpawn` instrumented.
#[test]
fn runtime_handle_spawn_with_targets_correct_runtime() {
    use dial9_tokio_telemetry::telemetry::TelemetryCore;

    let (writer, events) = common::CapturingWriter::new();
    let guard = TelemetryCore::builder().writer(writer).build().unwrap();
    guard.enable();

    let mut builder_a = tokio::runtime::Builder::new_multi_thread();
    builder_a.worker_threads(1).enable_all().thread_name("rt-a");
    let (rt_a, handle_a) = guard
        .trace_runtime("a")
        .task_tracking(true)
        .build(builder_a)
        .unwrap();

    let mut builder_b = tokio::runtime::Builder::new_multi_thread();
    builder_b.worker_threads(1).enable_all().thread_name("rt-b");
    let (rt_b, handle_b) = guard
        .trace_runtime("b")
        .task_tracking(true)
        .build(builder_b)
        .unwrap();

    let task_id_a: Arc<Mutex<Option<TaskId>>> = Arc::new(Mutex::new(None));
    let task_id_b: Arc<Mutex<Option<TaskId>>> = Arc::new(Mutex::new(None));

    let mut set_a: JoinSet<String> = JoinSet::new();
    let id_a = task_id_a.clone();
    handle_a.spawn_with(
        async move {
            *id_a.lock().unwrap() = tokio::task::try_id().map(TaskId::from);
            tokio::task::yield_now().await;
            std::thread::current().name().unwrap_or("?").to_string()
        },
        |f| set_a.spawn_on(f, rt_a.handle()),
    );

    let mut set_b: JoinSet<String> = JoinSet::new();
    let id_b = task_id_b.clone();
    handle_b.spawn_with(
        async move {
            *id_b.lock().unwrap() = tokio::task::try_id().map(TaskId::from);
            tokio::task::yield_now().await;
            std::thread::current().name().unwrap_or("?").to_string()
        },
        |f| set_b.spawn_on(f, rt_b.handle()),
    );

    let name_a = rt_a.block_on(async move { set_a.join_next().await.unwrap().unwrap() });
    let name_b = rt_b.block_on(async move { set_b.join_next().await.unwrap().unwrap() });

    assert!(name_a.starts_with("rt-a"), "got {name_a}");
    assert!(name_b.starts_with("rt-b"), "got {name_b}");

    drop(rt_a);
    drop(rt_b);
    let _ = guard.graceful_shutdown(Duration::from_secs(1));

    let task_id_a = task_id_a.lock().unwrap().expect("task id a captured");
    let task_id_b = task_id_b.lock().unwrap().expect("task id b captured");
    let events = events.lock().unwrap();

    for expected in [task_id_a, task_id_b] {
        let saw_instrumented_spawn = events.iter().any(|event| {
            matches!(
                event,
                TelemetryEvent::TaskSpawn {
                    task_id,
                    instrumented: Some(true),
                    ..
                } if *task_id == expected
            )
        });
        assert!(
            saw_instrumented_spawn,
            "expected instrumented TaskSpawn for runtime handle task {expected:?}"
        );

        let saw_wake = events.iter().any(|event| {
            matches!(event, TelemetryEvent::WakeEvent { woken_task_id, .. } if *woken_task_id == expected)
        });
        assert!(
            saw_wake,
            "expected WakeEvent for runtime handle task {expected:?}"
        );
    }
}
