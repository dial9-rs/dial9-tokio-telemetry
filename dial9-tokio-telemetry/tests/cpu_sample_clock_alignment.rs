//! Verify that CPU profile sample timestamps align with wall-clock timestamps,
//! and that samples are attributed to the correct worker thread.
//!
//! We spawn async tasks that sleep (150ms), burn CPU (80ms), then sleep again.
//! CPU samples must land within the burn windows. If the perf clock were
//! CLOCK_MONOTONIC_RAW instead of CLOCK_MONOTONIC, timestamps would be offset
//! by ~25ms and many samples would spill outside the expected windows.
//!
//! Worker attribution: because each task is awaited sequentially and the CPU
//! burn blocks the worker thread, all samples within a burn window must come
//! from the single worker that was running that task.  We discover that worker
//! by finding the `PollStart` event whose time interval (PollStart →
//! corresponding PollEnd) contains the burn window.

mod common;

#[cfg(feature = "cpu-profiling")]
#[test]
fn cpu_sample_timestamps_align_with_wall_clock() {
    use dial9_tokio_telemetry::telemetry::events::{CpuSampleSource, TelemetryEvent};
    use dial9_tokio_telemetry::telemetry::{CpuProfilingConfig, TracedRuntime};
    use std::sync::{Arc, Mutex};
    use std::time::Duration;

    let (writer, events) = common::CapturingWriter::new();

    let num_workers = 2;
    let mut builder = tokio::runtime::Builder::new_multi_thread();
    builder.worker_threads(num_workers).enable_all();

    let (runtime, guard) = TracedRuntime::builder()
        .with_cpu_profiling(CpuProfilingConfig {
            frequency_hz: 999,
            ..Default::default()
        })
        .build_and_start(builder, Box::new(writer))
        .unwrap();

    // Use the guard's start_time so we're in the same clock domain as trace timestamps.
    let trace_start = guard.start_time();
    let burn_windows: Arc<Mutex<Vec<(u64, u64)>>> = Arc::new(Mutex::new(Vec::new()));

    // Pattern: 150ms sleep → 80ms burn → 150ms sleep, repeated sequentially.
    // The 150ms gaps are much larger than the ~25ms MONOTONIC_RAW offset,
    // so a clock mismatch will cause samples to spill outside burn windows.
    runtime.block_on(async {
        for _ in 0..3u64 {
            let windows = burn_windows.clone();
            let ts = trace_start;
            tokio::spawn(async move {
                std::thread::sleep(Duration::from_millis(150));
                let before = ts.elapsed().as_nanos() as u64;
                burn_cpu(Duration::from_millis(80));
                let after = ts.elapsed().as_nanos() as u64;
                std::thread::sleep(Duration::from_millis(150));
                windows.lock().unwrap().push((before, after));
            })
            .await
            .unwrap();
        }
        // Let flush cycle pick up all samples
        tokio::time::sleep(Duration::from_millis(500)).await;
    });

    drop(runtime);
    drop(guard);

    let events = events.lock().unwrap();
    let windows = burn_windows.lock().unwrap();

    // ── Extract CPU samples ────────────────────────────────────────────────
    let cpu_samples: Vec<(u64, usize)> = events
        .iter()
        .filter_map(|e| match e {
            TelemetryEvent::CpuSample {
                timestamp_nanos,
                worker_id,
                source: CpuSampleSource::CpuProfile,
                ..
            } => Some((*timestamp_nanos, *worker_id)),
            _ => None,
        })
        .collect();

    let cpu_ts: Vec<u64> = cpu_samples.iter().map(|&(t, _)| t).collect();

    if common::is_ci() {
        eprintln!("we don't actually capture sample events in CI");
        return;
    }

    assert!(!cpu_ts.is_empty(), "expected CPU profile samples");

    // ── Build poll intervals: (poll_start_ns, poll_end_ns, worker_id) ─────
    //
    // We track open PollStart events per worker and close them when we see a
    // matching PollEnd.  Each closed interval is recorded so we can later
    // identify which worker was running during each burn window.
    let mut poll_intervals: Vec<(u64, u64, usize)> = Vec::new();
    {
        // worker_id → poll_start_ns
        let mut open: std::collections::HashMap<usize, u64> = std::collections::HashMap::new();
        for event in events.iter() {
            match event {
                TelemetryEvent::PollStart {
                    timestamp_nanos,
                    worker_id,
                    ..
                } => {
                    open.insert(*worker_id, *timestamp_nanos);
                }
                TelemetryEvent::PollEnd {
                    timestamp_nanos,
                    worker_id,
                } => {
                    if let Some(start) = open.remove(worker_id) {
                        poll_intervals.push((start, *timestamp_nanos, *worker_id));
                    }
                }
                _ => {}
            }
        }
    }

    // ── For each burn window, find the worker that owned it ───────────────
    //
    // The burn window must be fully contained within a single poll interval
    // because `std::thread::sleep` blocks the worker and tokio does not
    // preempt it to another task.  We use a small slack to account for the
    // fact that PollStart/PollEnd timestamps are recorded just outside the
    // actual burn.
    let slack_ns = 1_000_000u64; // 1 ms

    for (i, &(burn_start, burn_end)) in windows.iter().enumerate() {
        // ── Timestamp alignment: at least one sample must fall in the window ──
        let in_window: Vec<(u64, usize)> = cpu_samples
            .iter()
            .filter(|&&(t, _)| t >= burn_start.saturating_sub(slack_ns) && t <= burn_end + slack_ns)
            .copied()
            .collect();

        let closest = cpu_ts
            .iter()
            .map(|&t| {
                if t < burn_start {
                    burn_start - t
                } else if t > burn_end {
                    t - burn_end
                } else {
                    0
                }
            })
            .min()
            .unwrap_or(u64::MAX);

        eprintln!(
            "burn window {i}: [{burn_start}..{burn_end}] ({}ms), {} samples in window, closest: {}us",
            (burn_end - burn_start) / 1_000_000,
            in_window.len(),
            closest / 1_000,
        );

        assert!(
            !in_window.is_empty(),
            "burn window {i} ({}ms) had no CPU samples within {slack_ns}ns slack.\n\
             closest was {closest}ns away.\n\
             timestamps (first 20): {:?}",
            (burn_end - burn_start) / 1_000_000,
            &cpu_ts[..cpu_ts.len().min(20)]
        );

        // ── Worker attribution: find the poll interval covering this burn ──
        //
        // We extend both ends of the burn window by `slack_ns` when checking
        // containment so that scheduling jitter at the boundary doesn't cause
        // a false miss.
        let owning_poll = poll_intervals
            .iter()
            .find(|&&(ps, pe, _)| ps <= burn_start + slack_ns && pe + slack_ns >= burn_end);

        if let Some(&(ps, pe, expected_worker)) = owning_poll {
            eprintln!(
                "  burn window {i} owned by worker {expected_worker} \
                 (poll [{ps}..{pe}])"
            );

            // Every sample that falls inside this burn window must come from
            // `expected_worker`.  A sample from a different worker would mean
            // the profiler mis-attributed the sample.
            for &(t, w) in &in_window {
                assert_eq!(
                    w, expected_worker,
                    "burn window {i}: CPU sample at {t}ns was attributed to worker \
                     {w} but the task was running on worker {expected_worker}.\n\
                     burn window: [{burn_start}..{burn_end}]\n\
                     poll interval: [{ps}..{pe}]"
                );
            }
        } else {
            panic!(
                "burn window {i} [{burn_start}..{burn_end}] \
                 could not be matched to a poll interval; \
                 skipping worker attribution check for this window.\n\
                 available poll intervals: {poll_intervals:?}"
            );
        }
    }

    // ── Global: ≥90 % of all CPU samples must fall within a burn window ───
    // With correct clocks, nearly 100 % will. With a 25 ms offset, many spill.
    let total = cpu_ts.len();
    let in_any_window = cpu_ts
        .iter()
        .filter(|&&t| {
            windows
                .iter()
                .any(|&(s, e)| t >= s.saturating_sub(slack_ns) && t <= e + slack_ns)
        })
        .count();
    let pct = in_any_window as f64 / total as f64 * 100.0;
    eprintln!("{in_any_window}/{total} samples in burn windows ({pct:.1}%)");
    assert!(
        pct >= 90.0,
        "only {pct:.1}% of CPU samples fell within burn windows (expected >=90%). \
         likely clock domain mismatch (CLOCK_MONOTONIC_RAW vs CLOCK_MONOTONIC)"
    );

    // ── All samples must lie within the total test duration ───────────────
    let test_duration_ns = trace_start.elapsed().as_nanos() as u64;
    for &t in &cpu_ts {
        assert!(
            t <= test_duration_ns + slack_ns,
            "CPU sample timestamp {t}ns exceeds test duration {test_duration_ns}ns"
        );
    }
}

#[cfg(feature = "cpu-profiling")]
fn burn_cpu(duration: std::time::Duration) {
    let start = std::time::Instant::now();
    let mut x: u64 = 1;
    while start.elapsed() < duration {
        for _ in 0..1000 {
            x = x.wrapping_mul(6364136223846793005).wrapping_add(1);
        }
        std::hint::black_box(x);
    }
}

/// Verify that `ThreadNameDef` events are emitted for non-worker threads:
/// - A plain `std::thread` spawned outside the runtime
/// - A `spawn_blocking` task running on tokio's blocking pool
///
/// Both threads burn CPU for longer than the 250ms flush interval, then exit.
/// The flush cycle's `drain()` eagerly caches thread names from
/// `/proc/self/task/<tid>/comm` while the threads are still alive. The final
/// `ThreadNameDef` emission happens later at write time, after the threads
/// have exited — proving the eager cache is necessary.
#[cfg(feature = "cpu-profiling")]
#[test]
fn thread_name_attribution_for_external_and_blocking_threads() {
    use dial9_tokio_telemetry::telemetry::events::{TelemetryEvent, UNKNOWN_WORKER};
    use dial9_tokio_telemetry::telemetry::{CpuProfilingConfig, TracedRuntime};
    use std::time::Duration;

    let (writer, events) = common::CapturingWriter::new();

    let mut builder = tokio::runtime::Builder::new_multi_thread();
    builder.worker_threads(2).enable_all();

    let (runtime, guard) = TracedRuntime::builder()
        .with_cpu_profiling(CpuProfilingConfig {
            frequency_hz: 999,
            ..Default::default()
        })
        .build_and_start(builder, Box::new(writer))
        .unwrap();

    // ── std::thread with a known name — exits before flush ───────────────
    let ext_handle = std::thread::Builder::new()
        .name("my-ext-thread".into())
        .spawn(|| burn_cpu(Duration::from_millis(400)))
        .unwrap();

    // ── spawn_blocking — also exits before flush ─────────────────────────
    let blocking_handle = runtime.spawn(async {
        tokio::task::spawn_blocking(|| burn_cpu(Duration::from_millis(400)));
        tokio::task::spawn_blocking(|| burn_cpu(Duration::from_millis(400)))
            .await
            .unwrap();
    });

    // Wait for both to finish — threads are gone after this point
    ext_handle.join().unwrap();
    runtime.block_on(async {
        blocking_handle.await.unwrap();
        // Now let the flush cycle run. The threads have already exited,
        // so /proc/self/task/<tid>/comm is no longer readable. Thread names
        // must come from the eager cache populated during drain().
        tokio::time::sleep(Duration::from_millis(500)).await;
    });

    drop(runtime);
    drop(guard);

    if common::is_ci() {
        eprintln!("on CI, this test can run, but we don't get these events");
        return;
    }

    let events = events.lock().unwrap();

    // ── Collect ThreadNameDef events ─────────────────────────────────────
    let thread_defs: Vec<(u32, &str)> = events
        .iter()
        .filter_map(|e| match e {
            TelemetryEvent::ThreadNameDef { tid, name } => Some((*tid, name.as_str())),
            _ => None,
        })
        .collect();

    eprintln!("ThreadNameDef events: {thread_defs:?}");

    // ── Verify the external thread name appears ──────────────────────────
    let ext_def = thread_defs
        .iter()
        .find(|(_, name)| *name == "my-ext-thread");
    assert!(
        ext_def.is_some(),
        "expected ThreadNameDef for 'my-ext-thread', got: {thread_defs:?}"
    );
    let ext_tid = ext_def.unwrap().0;

    // ── Verify a tokio blocking-thread name appears ──────────────────────
    // spawn_blocking threads are named "tokio-runtime-w" by tokio
    let blocking_def = thread_defs
        .iter()
        .find(|(_, name)| name.starts_with("tokio-runtime-w"));
    assert!(
        blocking_def.is_some(),
        "expected ThreadNameDef for a tokio blocking thread (tokio-runtime-w*), got: {thread_defs:?}"
    );
    let blocking_tid = blocking_def.unwrap().0;

    // ── Verify CpuSamples exist for both tids with UNKNOWN_WORKER ────────
    let ext_samples: Vec<_> = events
        .iter()
        .filter(|e| matches!(e, TelemetryEvent::CpuSample { tid, worker_id, .. } if *tid == ext_tid && *worker_id == UNKNOWN_WORKER))
        .collect();
    eprintln!(
        "CPU samples for ext thread (tid={ext_tid}): {}",
        ext_samples.len()
    );
    assert!(
        !ext_samples.is_empty(),
        "expected CPU samples for external thread tid={ext_tid}"
    );

    let blocking_samples: Vec<_> = events
        .iter()
        .filter(|e| matches!(e, TelemetryEvent::CpuSample { tid, worker_id, .. } if *tid == blocking_tid && *worker_id == UNKNOWN_WORKER))
        .collect();
    eprintln!(
        "CPU samples for blocking thread (tid={blocking_tid}): {}",
        blocking_samples.len()
    );
    assert!(
        !blocking_samples.is_empty(),
        "expected CPU samples for blocking thread tid={blocking_tid}"
    );
}
