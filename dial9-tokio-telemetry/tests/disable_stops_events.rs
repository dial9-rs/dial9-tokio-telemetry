mod common;

use dial9_tokio_telemetry::telemetry::{RotatingWriter, TelemetryEvent, TracedRuntime};
use std::time::Duration;

/// After `disable()` is called and in-flight events are drained, no new
/// events should be produced by subsequent work.
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

    // Wait for the flush thread to drain any in-flight events produced
    // before disable.
    std::thread::sleep(Duration::from_millis(200));

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
    std::thread::sleep(Duration::from_millis(500));

    let count_after_phase2 = events.lock().unwrap().len();

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

/// After `disable()` with CPU profiling enabled, no new events should
/// be produced — including CPU samples.
///
/// Linux-only: CPU profiling requires `perf_event_open`.
#[test]
#[cfg(all(feature = "cpu-profiling", target_os = "linux"))]
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

    // Phase 1: burn CPU to generate perf samples while enabled.
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
        // Wait for flush thread to drain CPU samples (~1s self-drain interval).
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

    // Wait for in-flight CPU samples to drain.
    std::thread::sleep(Duration::from_millis(1500));

    let total_after_disable = events.lock().unwrap().len();

    // Phase 2: burn CPU while disabled — should NOT produce any events
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

/// After `disable()`, the RotatingWriter must not produce new segments.
///
/// Uses a 1-second rotation period and waits 5 seconds after disable.
/// If the flush loop were still driving rotation, we'd see new `.bin`
/// files appear.
#[test]
fn disable_stops_segment_rotation() {
    let dir = tempfile::tempdir().unwrap();
    let trace_path = dir.path().join("trace.bin");

    let writer = RotatingWriter::builder()
        .base_path(&trace_path)
        .max_file_size(100 * 1024 * 1024)
        .max_total_size(500 * 1024 * 1024)
        .rotation_period(Duration::from_secs(1))
        .build()
        .unwrap();

    let mut builder = tokio::runtime::Builder::new_multi_thread();
    builder.worker_threads(2).enable_all();

    let (runtime, guard) = TracedRuntime::builder()
        .with_trace_path(trace_path.to_str().unwrap())
        .build_and_start(builder, writer)
        .unwrap();

    let handle = guard.handle();

    // Phase 1: produce events while enabled, let a few rotations happen.
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
        // Wait for at least one rotation (1s period).
        tokio::time::sleep(Duration::from_millis(2500)).await;
    });

    // Disable telemetry
    handle.disable();

    // Wait for in-flight events to drain.
    std::thread::sleep(Duration::from_millis(200));

    // Count segments on disk after disable.
    let count_segments = || -> usize {
        std::fs::read_dir(dir.path())
            .unwrap()
            .filter_map(|e| e.ok())
            .filter(|e| {
                let p = e.path();
                p.extension()
                    .is_some_and(|ext| ext == "bin" || ext == "active" || ext == "gz")
            })
            .count()
    };

    let segments_after_disable = count_segments();

    // Wait 5 seconds — if rotation were still happening with a 1s period,
    // we'd see ~5 new segments.
    std::thread::sleep(Duration::from_secs(5));

    let segments_after_wait = count_segments();

    assert_eq!(
        segments_after_disable,
        segments_after_wait,
        "expected no new segments after disable(), but got {} new segments \
         (before={segments_after_disable}, after={segments_after_wait})",
        segments_after_wait.saturating_sub(segments_after_disable),
    );

    drop(runtime);
    let _ = guard.graceful_shutdown(Duration::from_secs(2));
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
