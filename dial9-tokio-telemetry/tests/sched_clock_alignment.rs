//! Verify that sched event timestamps (from perf) align with wall-clock
//! timestamps from `clock_monotonic_ns()` (CLOCK_MONOTONIC).
//!
//! We spawn async tasks that call `std::thread::sleep` for known durations,
//! recording wall-clock timestamps around each sleep. Sched events (context
//! switches) must land within the sleep windows — if the perf clock were
//! CLOCK_MONOTONIC_RAW instead of CLOCK_MONOTONIC, timestamps would drift
//! and events would fall outside the expected windows.

mod common;

#[cfg(feature = "cpu-profiling")]
#[test]
fn sched_event_timestamps_align_with_wall_clock() {
    use dial9_tokio_telemetry::telemetry::events::{CpuSampleSource, RawEvent, clock_monotonic_ns};
    use dial9_tokio_telemetry::telemetry::{SchedEventConfig, TracedRuntime};
    use std::sync::{Arc, Mutex};
    use std::time::Duration;

    use crate::common::CapturingWriter;

    let (writer, events) = CapturingWriter::new();

    let num_workers = 2;
    let mut builder = tokio::runtime::Builder::new_multi_thread();
    builder.worker_threads(num_workers).enable_all();

    let (runtime, guard) = TracedRuntime::builder()
        .with_sched_events(SchedEventConfig::default())
        .build_and_start(builder, writer)
        .unwrap();

    // All timestamps are now absolute CLOCK_MONOTONIC nanoseconds.
    let _trace_start = guard.start_time();
    let sleep_windows: Arc<Mutex<Vec<(u64, u64)>>> = Arc::new(Mutex::new(Vec::new()));

    let sleep_duration = Duration::from_millis(1);
    let num_sleeps = 4;

    runtime.block_on(async {
        // Warmup: ensure all worker threads are fully initialized and perf
        // events are flowing before we start measuring. Without this, the
        // first sleep window can race with worker thread startup — Tokio
        // spawns workers via the blocking pool and `set_thread_id` /
        // `resolve_worker_id` may not have run yet, causing sched events
        // to be attributed to `Blocking` instead of `Worker(i)`.
        for _ in 0..num_workers {
            tokio::spawn(async {
                std::thread::sleep(Duration::from_millis(10));
            })
            .await
            .unwrap();
        }
        // Let the flush cycle drain warmup sched events
        tokio::time::sleep(Duration::from_millis(100)).await;

        // Space out sleeps so windows don't overlap
        for i in 0..num_sleeps {
            let windows = sleep_windows.clone();
            tokio::spawn(async move {
                // Stagger starts so sleeps don't all overlap
                tokio::time::sleep(Duration::from_millis(i * 100)).await;
                let before = clock_monotonic_ns();
                std::thread::sleep(sleep_duration);
                let after = clock_monotonic_ns();
                windows.lock().unwrap().push((before, after));
            })
            .await
            .unwrap();
        }
        // Let flush cycle pick up all sched events
        tokio::time::sleep(Duration::from_millis(500)).await;
    });

    drop(runtime);
    drop(guard);

    let events = events.lock().unwrap();
    let windows = sleep_windows.lock().unwrap();

    // Collect sched event timestamps attributed to workers
    let sched_timestamps: Vec<u64> = events
        .iter()
        .filter_map(|e| match e {
            RawEvent::CpuSample(data)
                if data.source == CpuSampleSource::SchedEvent
                    && data.worker_id.as_u64() < num_workers as u64 =>
            {
                Some(data.timestamp_nanos)
            }
            _ => None,
        })
        .collect();

    assert!(
        !sched_timestamps.is_empty(),
        "expected sched events attributed to workers"
    );

    // For each sleep window, at least one sched event should fall within it
    // (the thread going off-CPU when it calls sleep). We allow 1ms of slack
    // for scheduling jitter. Without the CLOCK_MONOTONIC fix in the sampler,
    // the offset is ~25ms on typical machines, so this easily catches regressions.
    let slack_ns = 1_000_000; // 1ms
    for (i, &(start, end)) in windows.iter().enumerate() {
        let in_window: Vec<u64> = sched_timestamps
            .iter()
            .filter(|&&t| t >= start.saturating_sub(slack_ns) && t <= end + slack_ns)
            .copied()
            .collect();
        let closest = sched_timestamps
            .iter()
            .map(|&t| {
                if t < start {
                    start - t
                } else {
                    t.saturating_sub(end)
                }
            })
            .min()
            .unwrap_or(u64::MAX);
        eprintln!(
            "window {i}: [{start}..{end}] ({}ms), {} events in window, closest distance: {}us",
            (end - start) / 1_000_000,
            in_window.len(),
            closest / 1_000,
        );
        assert!(
            !in_window.is_empty(),
            "sleep window {i} [{start}..{end}] ({}ms) had no sched event within {slack_ns}ns slack.\n\
             closest sched event was {closest}ns away.\n\
             sched timestamps (first 20): {:?}",
            (end - start) / 1_000_000,
            &sched_timestamps[..sched_timestamps.len().min(20)]
        );
    }

    // Verify no sched events have wildly wrong timestamps (would indicate clock mismatch).
    // All sched events should be after trace start and before now.
    let now = clock_monotonic_ns();
    for &t in &sched_timestamps {
        assert!(
            t <= now + slack_ns,
            "sched event timestamp {t}ns exceeds current time {now}ns — \
             likely clock domain mismatch (CLOCK_MONOTONIC_RAW vs CLOCK_MONOTONIC)"
        );
    }
}
