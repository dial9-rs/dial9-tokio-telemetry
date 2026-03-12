use dial9_tokio_telemetry::telemetry::{
    TelemetryEvent, TraceReader, analyze_trace, compute_wake_to_poll_delays, detect_idle_workers,
    print_analysis,
};
use dial9_trace_format::InternedString;
use std::collections::HashMap;
use std::env;

fn main() {
    let args: Vec<String> = env::args().collect();
    if args.len() < 2 {
        eprintln!("Usage: {} <trace_file>", args[0]);
        std::process::exit(1);
    }

    let trace_file = &args[1];
    println!("Analyzing trace: {}", trace_file);

    let mut reader = TraceReader::new(trace_file).expect("Failed to open trace file");

    let (magic, version) = reader.read_header().expect("Failed to read header");
    println!("Magic: {}, Version: {}", magic, version);

    let events = reader.read_all().expect("Failed to read events");
    println!("Read {} events", events.len());
    if !reader.string_pool.is_empty() {
        println!("String pool entries: {}", reader.string_pool.len());
    }

    let analysis = analyze_trace(&events);
    print_analysis(&analysis, &reader.string_pool);

    println!("\n=== Idle Worker Detection ===");
    let idle_periods = detect_idle_workers(&events);

    let delays = compute_wake_to_poll_delays(&events);
    if !delays.is_empty() {
        let p50 = delays[delays.len() * 50 / 100];
        let p99 = delays[delays.len() * 99 / 100];
        let p999 = delays[delays.len() * 999 / 1000];
        let max = *delays.last().unwrap();
        println!("\n=== Wake→Poll Delays ({} samples) ===", delays.len());
        println!(
            "  p50: {:.1}µs, p99: {:.1}µs, p99.9: {:.1}µs, max: {:.1}µs",
            p50 as f64 / 1000.0,
            p99 as f64 / 1000.0,
            p999 as f64 / 1000.0,
            max as f64 / 1000.0,
        );
    }

    // Build task_id → spawn_loc_id from PollStart events
    let mut task_locs: HashMap<u32, InternedString> = HashMap::new();
    for e in &events {
        if let TelemetryEvent::PollStart {
            task_id,
            spawn_loc_id,
            ..
        } = e
        {
            task_locs
                .entry(task_id.to_u32())
                .or_insert(*spawn_loc_id);
        }
    }
    for (task_id, spawn_loc_id) in &reader.task_spawn_locs {
        task_locs
            .entry(task_id.to_u32())
            .or_insert(*spawn_loc_id);
    }

    // Count wakes by waker spawn location
    let mut wakes_by_loc: HashMap<Option<&str>, usize> = HashMap::new();
    let mut resolved = 0usize;
    let mut unresolved = 0usize;
    for e in &events {
        if let TelemetryEvent::WakeEvent { waker_task_id, .. } = e {
            let id = waker_task_id.to_u32();
            if id == 0 {
                *wakes_by_loc.entry(Some("<non-task context>")).or_default() += 1;
                resolved += 1;
            } else if let Some(loc_id) = task_locs.get(&id) {
                let loc = reader.string_pool.get(&loc_id.0);
                *wakes_by_loc.entry(loc.map(|s| s.as_str())).or_default() += 1;
                resolved += 1;
            } else {
                unresolved += 1;
            }
        }
    }
    if resolved + unresolved > 0 {
        println!(
            "\n=== Waker Identity ({} resolved, {} unresolved of {} total tasks in trace) ===",
            resolved,
            unresolved,
            task_locs.len()
        );

        let mut sorted: Vec<_> = wakes_by_loc.into_iter().collect();
        sorted.sort_by(|a, b| b.1.cmp(&a.1));
        for (loc, count) in sorted.iter().take(20) {
            println!("  {:>6} wakes — {}", count, loc.unwrap_or("?"));
        }
    }

    if !idle_periods.is_empty() {
        println!("\nIdle workers with queue pressure: {}", idle_periods.len());
        for (worker, duration, queue) in idle_periods.iter().take(5) {
            println!(
                "  Worker {} idle for {:.2}ms with global queue depth {}",
                worker,
                *duration as f64 / 1_000_000.0,
                queue
            );
        }
    }
}
