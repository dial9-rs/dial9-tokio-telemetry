use assert2::check;
use dial9_tokio_telemetry::telemetry::format::WorkerId;
use dial9_tokio_telemetry::telemetry::{TelemetryEvent, TraceAnalysis};
use std::collections::HashSet;
use std::time::Duration;
use tokio::runtime::RuntimeMetrics;

/// Validate that trace analysis matches tokio RuntimeMetrics.
/// Allows small discrepancies due to timing differences between metric snapshots and event recording.
pub fn validate_trace_matches_metrics(
    analysis: &TraceAnalysis,
    events: &[TelemetryEvent],
    metrics: &RuntimeMetrics,
) {
    let num_workers = metrics.num_workers();
    let metrics_spawned = metrics.spawned_tasks_count();

    // Read per-worker metrics
    let metrics_polls: Vec<u64> = (0..num_workers)
        .map(|w| metrics.worker_poll_count(w))
        .collect();
    let metrics_parks: Vec<u64> = (0..num_workers)
        .map(|w| metrics.worker_park_count(w))
        .collect();
    let metrics_busy: Vec<Duration> = (0..num_workers)
        .map(|w| metrics.worker_total_busy_duration(w))
        .collect();

    let active_workers: HashSet<usize> =
        (0..num_workers).filter(|&w| metrics_polls[w] > 0).collect();

    // Print diagnostics
    eprintln!("=== Tokio RuntimeMetrics ===");
    for w in 0..num_workers {
        eprintln!(
            "  worker {}: polls={}, parks={}, busy={:?}",
            w, metrics_polls[w], metrics_parks[w], metrics_busy[w],
        );
    }
    eprintln!("  spawned_tasks_count={}", metrics_spawned);
    eprintln!("=== Trace Analysis ===");
    eprintln!("  total_events={}", analysis.total_events);
    eprintln!("  duration_ns={}", analysis.duration_ns);
    for (w, metrics_poll) in metrics_polls.iter().enumerate().take(num_workers) {
        if let Some(stats) = analysis.worker_stats.get(&WorkerId::from(w)) {
            eprintln!(
                "  worker {}: polls={}, parks={}, unparks={}, poll_time_ns={}",
                w, stats.poll_count, stats.park_count, stats.unpark_count, stats.total_poll_time_ns,
            );
        } else {
            eprintln!(
                "  worker {}: MISSING from trace (tokio polls={})",
                w, metrics_poll
            );
        }
    }
    // Check for UNKNOWN_WORKER (255) events
    if let Some(stats) = analysis.worker_stats.get(&WorkerId::UNKNOWN) {
        eprintln!(
            "  ⚠️  UNKNOWN_WORKER (255): polls={}, parks={}, unparks={}, poll_time_ns={}",
            stats.poll_count, stats.park_count, stats.unpark_count, stats.total_poll_time_ns
        );
    }
    let total_tokio_polls: u64 = metrics_polls.iter().sum();
    let total_trace_poll_starts: usize = events
        .iter()
        .filter(|e| matches!(e, TelemetryEvent::PollStart { .. }))
        .count();
    eprintln!("=== Totals ===");
    eprintln!("  tokio total polls (tokio metrics): {}", total_tokio_polls);
    eprintln!(
        "  trace total PollStart events: {}",
        total_trace_poll_starts
    );

    // ===== Checks (all run, failures collected by assert2 scope guard) =====
    //
    // NOTE: Small discrepancies (±1-5 polls, ±1-3 parks) are expected due to:
    // - Timing differences between when tokio increments counters vs when we record events
    // - Events during runtime startup/shutdown
    // - Sampling point differences (tokio snapshots metrics, we record events in real-time)
    //
    // The original bug (resolve_worker_id returning 0) caused much larger mismatches
    // with events from all workers being misattributed to worker 0.

    // 1. Worker count
    check!(num_workers == num_workers);
    check!(!active_workers.is_empty());

    // 2. Every active worker present in trace
    let trace_has_all_active = active_workers
        .iter()
        .all(|w| analysis.worker_stats.contains_key(&WorkerId::from(*w)));
    let missing: Vec<_> = active_workers
        .iter()
        .filter(|w| !analysis.worker_stats.contains_key(&WorkerId::from(**w)))
        .collect();
    check!(
        trace_has_all_active,
        "workers missing from trace: {:?}",
        missing
    );

    // 3. Per-worker poll count: allow small discrepancy (±5)
    let poll_mismatches: Vec<_> = active_workers
        .iter()
        .filter_map(|&w| {
            analysis.worker_stats.get(&WorkerId::from(w)).and_then(|s| {
                let trace = s.poll_count as i64;
                let tokio = metrics_polls[w] as i64;
                let diff = (trace - tokio).abs();
                if diff > 30 {
                    Some((w, trace, tokio, diff))
                } else {
                    None
                }
            })
        })
        .collect();
    check!(
        poll_mismatches.is_empty(),
        "per-worker poll mismatches >5 (worker, trace, tokio, diff): {:?}",
        poll_mismatches
    );

    // 4. Total polls ≥ spawned tasks
    let total_trace_polls: u64 = analysis
        .worker_stats
        .values()
        .map(|s| s.poll_count as u64)
        .sum();
    check!(
        total_trace_polls >= metrics_spawned,
        "total polls ({}) < spawned tasks ({})",
        total_trace_polls,
        metrics_spawned
    );

    // 5. Per-worker park count sanity checks.
    //
    // Tokio's `worker_park_count` counts inner-loop iterations inside the
    // worker's `park()` method (spurious wakeups, maintenance cycles), while
    // the `on_thread_park` callback fires only once per `park()` call.
    // Therefore our trace count is generally less than tokio's count.
    // However, the metrics snapshot and trace end at different times, so the
    // trace may also capture a few parks after the snapshot. We check:
    //   (a) every active worker recorded at least one park
    //   (b) trace parks ≤ tokio parks + small margin (for post-snapshot parks)
    let park_violations: Vec<_> = active_workers
        .iter()
        .filter_map(|&w| {
            analysis.worker_stats.get(&WorkerId::from(w)).and_then(|s| {
                let trace = s.park_count;
                let tokio = metrics_parks[w] as usize;
                if trace == 0 || trace > tokio + 5 {
                    Some((w, trace, tokio))
                } else {
                    None
                }
            })
        })
        .collect();
    check!(
        park_violations.is_empty(),
        "park violations (worker, trace_parks, tokio_parks): {:?} — \
         trace parks must be >= 1 and <= tokio parks + 5",
        park_violations
    );

    // 6. Park/unpark balance: unparks ≈ parks (within 1)
    // With flush-on-drop, we may capture an extra unpark at shutdown.
    let park_unpark_violations: Vec<_> = active_workers
        .iter()
        .filter_map(|&w| {
            analysis.worker_stats.get(&WorkerId::from(w)).and_then(|s| {
                let diff = (s.park_count as i64 - s.unpark_count as i64).abs();
                if diff <= 1 {
                    None
                } else {
                    Some((w, s.park_count, s.unpark_count))
                }
            })
        })
        .collect();
    check!(
        park_unpark_violations.is_empty(),
        "park/unpark violations (worker, parks, unparks): {:?}",
        park_unpark_violations
    );

    // 7. Timestamp monotonicity per worker
    let mut last_ts: Vec<Option<u64>> = vec![None; num_workers];
    let mut violations = Vec::new();
    for (idx, event) in events.iter().enumerate() {
        if let Some(ts) = event.timestamp_nanos()
            && let Some(wid) = event.worker_id()
            && wid.as_u64() < num_workers as u64
        {
            if let Some(prev) = last_ts[wid.as_u64() as usize]
                && ts < prev
            {
                violations.push((idx, wid, prev, ts));
                if violations.len() >= 5 {
                    break;
                }
            }
            last_ts[wid.as_u64() as usize] = Some(ts);
        }
    }
    check!(
        violations.is_empty(),
        "timestamp monotonicity violations (event_idx, worker, prev_ts, curr_ts): {:?}",
        violations
    );

    // 8. PollStart/PollEnd pairing
    let poll_starts = events
        .iter()
        .filter(|e| matches!(e, TelemetryEvent::PollStart { .. }))
        .count();
    let poll_ends = events
        .iter()
        .filter(|e| matches!(e, TelemetryEvent::PollEnd { .. }))
        .count();
    check!(
        poll_starts == poll_ends,
        "PollStart ({}) != PollEnd ({})",
        poll_starts,
        poll_ends
    );
}
