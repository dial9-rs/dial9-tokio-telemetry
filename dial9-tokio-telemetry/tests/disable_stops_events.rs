mod common;

use dial9_tokio_telemetry::telemetry::{TelemetryEvent, TracedRuntime};
use std::time::Duration;

/// After `disable()` is called and in-flight events are drained, no new
/// events should be produced by subsequent work.
///
/// Strategy: enable → work → disable → wait for drain → snapshot →
/// more work → wait for drain → assert no new events.
#[test]
fn disable_stops_all_event_production() {
    let (writer, events) = common::CapturingWriter::new();

    let mut builder = tokio::runtime::Builder::new_multi_thread();
    builder.worker_threads(2).enable_all();

    let (runtime, guard) = TracedRuntime::builder()
        .with_task_tracking(true)
        .build_and_start(builder, writer)
        .unwrap();

    let handle = guard.handle();

    // Phase 1: produce events while enabled
    runtime.block_on(async {
        let mut handles = Vec::new();
        for _ in 0..100 {
            handles.push(tokio::spawn(async {
                tokio::task::yield_now().await;
            }));
        }
        for h in handles {
            h.await.unwrap();
        }
    });

    // Disable telemetry
    handle.disable();

    // Wait for the flush thread to drain any in-flight events that were
    // produced before disable. The flush thread cycles every 5ms and
    // drains TL buffers when the drain epoch is bumped (every ~30s or
    // on rotation). We sleep long enough for several flush cycles to
    // pick up whatever was already in the collector.
    std::thread::sleep(Duration::from_millis(200));

    // Snapshot: everything produced before disable should be flushed by now.
    let count_after_disable = events.lock().unwrap().len();

    // Phase 2: produce more work while disabled
    runtime.block_on(async {
        let mut handles = Vec::new();
        for _ in 0..500 {
            handles.push(tokio::spawn(async {
                for _ in 0..10 {
                    tokio::task::yield_now().await;
                }
            }));
        }
        for h in handles {
            h.await.unwrap();
        }
    });

    // Give the flush thread plenty of time to pick up any leaked events.
    // If the enabled check is working, nothing new should appear.
    std::thread::sleep(Duration::from_millis(500));

    let count_after_phase2 = events.lock().unwrap().len();

    // No new events should have been produced after disable
    assert_eq!(
        count_after_disable,
        count_after_phase2,
        "expected no new events after disable(), but got {} new events \
         (before={count_after_disable}, after={count_after_phase2})",
        count_after_phase2 - count_after_disable,
    );

    drop(runtime);
    drop(guard);
}

/// After `disable()` with CPU profiling enabled, no CPU sample events
/// should be produced.
///
/// This is the key regression test: `flush_cpu()` in EventWriter calls
/// `buffer::record_event()` directly, bypassing the `enabled` check in
/// `SharedState::record_encodable_event()`.
#[test]
#[cfg(feature = "cpu-profiling")]
fn disable_stops_cpu_sample_production() {
    use dial9_tokio_telemetry::telemetry::cpu_profile::CpuProfilingConfig;

    let (writer, events) = common::CapturingWriter::new();

    let mut builder = tokio::runtime::Builder::new_multi_thread();
    builder.worker_threads(2).enable_all();

    let (runtime, guard) = TracedRuntime::builder()
        .with_task_tracking(true)
        .with_cpu_profiling(CpuProfilingConfig::default())
        .build_and_start_with_writer(builder, writer)
        .unwrap();

    let handle = guard.handle();

    // Phase 1: produce CPU samples while enabled.
    // Burn CPU to generate perf samples, then wait for the flush thread
    // to drain them (flush_cpu runs every 5ms, self-drain every ~1s).
    runtime.block_on(async {
        let mut handles = Vec::new();
        for _ in 0..4 {
            handles.push(tokio::spawn(async {
                let start = std::time::Instant::now();
                while start.elapsed() < Duration::from_millis(500) {
                    std::hint::black_box(0u64.wrapping_mul(42));
                }
            }));
        }
        for h in handles {
            h.await.unwrap();
        }
        // Wait for flush thread to drain CPU samples through the writer.
        // The flush thread's own TL buffer is drained every ~1s (200 cycles × 5ms).
        tokio::time::sleep(Duration::from_millis(1500)).await;
    });

    let cpu_samples_phase1 = events
        .lock()
        .unwrap()
        .iter()
        .filter(|e| matches!(e, TelemetryEvent::CpuSample { .. }))
        .count();
    assert!(
        cpu_samples_phase1 > 0,
        "phase 1 should produce CPU samples (got 0). Is perf_event_paranoid <= 2?"
    );

    // Disable telemetry
    handle.disable();

    // Wait for in-flight events to drain. CPU samples from the last
    // flush_cpu call sit in the flush thread's TL buffer until the next
    // self-drain (~1s). Give it enough time to fully drain.
    std::thread::sleep(Duration::from_millis(1500));

    // Snapshot total events after disable
    let total_after_disable = events.lock().unwrap().len();

    // Phase 2: burn CPU while disabled — should NOT produce CPU samples
    runtime.block_on(async {
        let mut handles = Vec::new();
        for _ in 0..4 {
            handles.push(tokio::spawn(async {
                let start = std::time::Instant::now();
                while start.elapsed() < Duration::from_millis(500) {
                    std::hint::black_box(0u64.wrapping_mul(42));
                }
            }));
        }
        for h in handles {
            h.await.unwrap();
        }
        // Give the flush thread time to pick up any leaked CPU samples
        tokio::time::sleep(Duration::from_millis(500)).await;
    });

    let total_after_phase2 = events.lock().unwrap().len();

    assert_eq!(
        total_after_disable,
        total_after_phase2,
        "expected no new events after disable() with CPU profiling, \
         but got {} new events (before={total_after_disable}, after={total_after_phase2})",
        total_after_phase2 - total_after_disable,
    );

    drop(runtime);
    drop(guard);
}

/// After `disable()`, re-enabling with `enable()` should resume event production.
#[test]
fn enable_after_disable_resumes_events() {
    let (writer, events) = common::CapturingWriter::new();

    let mut builder = tokio::runtime::Builder::new_multi_thread();
    builder.worker_threads(2).enable_all();

    let (runtime, guard) = TracedRuntime::builder()
        .with_task_tracking(true)
        .build_and_start(builder, writer)
        .unwrap();

    let handle = guard.handle();

    // Disable, then re-enable
    handle.disable();
    std::thread::sleep(Duration::from_millis(50));
    handle.enable();

    runtime.block_on(async {
        let mut handles = Vec::new();
        for _ in 0..100 {
            handles.push(tokio::spawn(async {
                tokio::task::yield_now().await;
            }));
        }
        for h in handles {
            h.await.unwrap();
        }
    });

    // Drop runtime to flush TL buffers, then guard to flush collector
    drop(runtime);
    drop(guard);

    let final_events = events.lock().unwrap();
    let runtime_event_count = final_events
        .iter()
        .filter(|e| {
            matches!(
                e,
                TelemetryEvent::PollStart { .. }
                    | TelemetryEvent::PollEnd { .. }
                    | TelemetryEvent::WorkerPark { .. }
                    | TelemetryEvent::WorkerUnpark { .. }
            )
        })
        .count();
    assert!(
        runtime_event_count > 0,
        "re-enabling after disable should resume event production, got 0 runtime events"
    );
}
