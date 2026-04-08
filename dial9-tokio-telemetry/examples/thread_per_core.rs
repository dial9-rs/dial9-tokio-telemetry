//! Thread-per-core architecture using current-thread runtimes.
//!
//! Some applications pin one single-threaded tokio runtime per CPU core for
//! cache locality and predictable latency. This example shows how to trace
//! that pattern: one primary runtime owns the telemetry session, and each
//! per-core runtime attaches via `build_with_reuse`.
//!
//! After the workload completes, the trace file is read back and all
//! PollStart/PollEnd worker IDs are printed alongside the runtime→worker
//! mapping from segment metadata — verifying that every core's events
//! landed in the trace with the correct identity.
//!
//! Usage:
//!   cargo run --example thread_per_core
//!
//! After running, inspect the trace:
//!   cargo run --example analyze_trace -- /tmp/thread_per_core/trace.0.bin

use dial9_tokio_telemetry::telemetry::{
    RotatingWriter, TelemetryEvent, TracedRuntime, WorkerId, format,
};
use std::collections::{BTreeMap, BTreeSet};
use std::time::Duration;

fn main() -> std::io::Result<()> {
    let trace_dir = "/tmp/thread_per_core";
    let _ = std::fs::create_dir_all(trace_dir);
    // Clean up previous runs.
    for entry in std::fs::read_dir(trace_dir)? {
        let entry = entry?;
        if entry
            .path()
            .extension()
            .is_some_and(|e| e == "bin" || e == "active")
        {
            std::fs::remove_file(entry.path())?;
        }
    }

    let writer = RotatingWriter::builder()
        .base_path(format!("{trace_dir}/trace.bin"))
        .max_file_size(1024 * 1024)
        .max_total_size(5 * 1024 * 1024)
        .build()?;

    // A small multi-thread runtime to own the telemetry session (flush thread,
    // writer, etc.). In a real app this might be your main runtime.
    let mut builder = tokio::runtime::Builder::new_multi_thread();
    builder.worker_threads(1).enable_all();

    let (coordinator_rt, guard) = TracedRuntime::builder()
        .with_runtime_name("coordinator")
        .with_trace_path(format!("{trace_dir}/trace.bin"))
        .build_and_start(builder, writer)?;

    // Spawn one current-thread runtime per core.
    let num_cores = std::thread::available_parallelism()
        .map(|n| n.get().min(4)) // cap at 4 for the demo
        .unwrap_or(2);

    println!("Spawning {num_cores} current-thread runtimes...");

    let threads: Vec<_> = (0..num_cores)
        .map(|core_id| {
            let mut core_builder = tokio::runtime::Builder::new_current_thread();
            core_builder.enable_all();

            let core_rt = TracedRuntime::builder()
                .with_runtime_name(format!("core-{core_id}"))
                .build_with_reuse(core_builder, &guard)
                .unwrap();

            std::thread::Builder::new()
                .name(format!("core-{core_id}"))
                .spawn(move || {
                    core_rt
                        .block_on(core_rt.spawn(async move {
                            for i in 0..20 {
                                tokio::task::yield_now().await;
                                tokio::time::sleep(Duration::from_millis(2)).await;
                                if i % 10 == 0 {
                                    println!("  [core-{core_id}] processed {i}");
                                }
                            }
                        }))
                        .unwrap();
                })
                .unwrap()
        })
        .collect();

    // Let the coordinator do some light work too.
    coordinator_rt.block_on(async {
        tokio::time::sleep(Duration::from_millis(100)).await;
        println!("  [coordinator] health check done");
    });

    for t in threads {
        t.join().unwrap();
    }
    println!("All cores finished.\n");

    drop(coordinator_rt);
    let _ = guard.graceful_shutdown(Duration::from_secs(5));

    // ── Read back the trace and verify ──────────────────────────────────

    let mut files: Vec<_> = std::fs::read_dir(trace_dir)?
        .filter_map(|e| e.ok())
        .map(|e| e.path())
        .filter(|p| p.extension().is_some_and(|ext| ext == "bin"))
        .collect();
    files.sort();

    // Collect: runtime name → worker IDs (from metadata), and
    //          worker ID → poll event count (from PollStart/PollEnd).
    let mut runtime_workers: BTreeMap<String, Vec<u64>> = BTreeMap::new();
    let mut poll_counts: BTreeMap<u64, usize> = BTreeMap::new();
    let mut seen_workers: BTreeSet<u64> = BTreeSet::new();
    let mut total_polls = 0usize;
    let mut unknown_polls = 0usize;

    for file in &files {
        let data = std::fs::read(file)?;
        let events = format::decode_events(&data)?;
        for event in &events {
            match event {
                TelemetryEvent::SegmentMetadata { entries, .. } => {
                    for (key, val) in entries {
                        if let Some(name) = key.strip_prefix("runtime.") {
                            let ids: Vec<u64> =
                                val.split(',').filter_map(|s| s.parse().ok()).collect();
                            runtime_workers.insert(name.to_string(), ids);
                        }
                    }
                }
                TelemetryEvent::PollStart { worker_id, .. }
                | TelemetryEvent::PollEnd { worker_id, .. } => {
                    total_polls += 1;
                    if *worker_id != WorkerId::UNKNOWN {
                        let id = worker_id.as_u64();
                        seen_workers.insert(id);
                        *poll_counts.entry(id).or_default() += 1;
                    } else {
                        unknown_polls += 1;
                    }
                }
                _ => {}
            }
        }
    }

    println!("=== Poll event summary ===");
    println!(
        "  total: {total_polls}, resolved: {}, unknown: {unknown_polls}",
        total_polls - unknown_polls
    );

    println!("=== Runtime → Worker mapping (from segment metadata) ===");
    for (name, ids) in &runtime_workers {
        println!("  {name}: workers {ids:?}");
    }

    println!("\n=== Poll events per worker ===");
    for (worker_id, count) in &poll_counts {
        // Find which runtime this worker belongs to.
        let runtime = runtime_workers
            .iter()
            .find(|(_, ids)| ids.contains(worker_id))
            .map(|(name, _)| name.as_str())
            .unwrap_or("unknown");
        println!("  worker {worker_id} ({runtime}): {count} poll events");
    }

    // Verify every worker that emitted events is accounted for in metadata.
    let metadata_ids: BTreeSet<u64> = runtime_workers.values().flatten().copied().collect();
    let unaccounted: Vec<_> = seen_workers.difference(&metadata_ids).collect();
    if unaccounted.is_empty() {
        println!("\n✓ All worker IDs are accounted for in runtime metadata.");
    } else {
        println!("\n✗ Workers not in metadata: {unaccounted:?}");
    }

    Ok(())
}
