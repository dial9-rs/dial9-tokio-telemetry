use dial9_tokio_telemetry::telemetry::{
    TelemetryEvent, TraceReader, analyze_trace, compute_wake_to_poll_delays, detect_idle_workers,
    print_analysis,
};
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
    if !reader.spawn_locations.is_empty() {
        println!("Spawn locations: {}", reader.spawn_locations.len());
    }

    let analysis = analyze_trace(&events);
    print_analysis(&analysis, &reader.spawn_locations);

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

    // Build task_id → spawn_loc_id from PollStart events (more complete than TaskSpawn alone)
    let mut task_locs: HashMap<u32, u16> = HashMap::new();
    for e in &events {
        if let TelemetryEvent::PollStart {
            task_id,
            spawn_loc_id,
            ..
        } = e
        {
            task_locs
                .entry(task_id.to_u32())
                .or_insert(spawn_loc_id.as_u16());
        }
    }
    // Also include TaskSpawn mappings from the reader
    for (task_id, spawn_loc_id) in &reader.task_spawn_locs {
        task_locs
            .entry(task_id.to_u32())
            .or_insert(spawn_loc_id.as_u16());
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
                let loc = reader
                    .spawn_locations
                    .get(&dial9_tokio_telemetry::telemetry::SpawnLocationId::from_u16(*loc_id));
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

        // Debug: show some unresolved waker task IDs vs known task IDs
        let mut unresolved_ids: HashMap<u32, usize> = HashMap::new();
        for e in &events {
            if let TelemetryEvent::WakeEvent { waker_task_id, .. } = e {
                let id = waker_task_id.to_u32();
                if id != 0 && !task_locs.contains_key(&id) {
                    *unresolved_ids.entry(id).or_default() += 1;
                }
            }
        }
        let mut top_unresolved: Vec<_> = unresolved_ids.into_iter().collect();
        top_unresolved.sort_by(|a, b| b.1.cmp(&a.1));
        println!("  Top unresolved waker IDs:");
        for (id, count) in top_unresolved.iter().take(5) {
            println!("    0x{:08x} ({}) — {} wakes", id, id, count);
        }
        println!("  Known task IDs (sample):");
        for (id, loc_id) in task_locs.iter().take(5) {
            let loc = reader
                .spawn_locations
                .get(&dial9_tokio_telemetry::telemetry::SpawnLocationId::from_u16(*loc_id));
            println!(
                "    0x{:08x} ({}) — {}",
                id,
                id,
                loc.map(|s| s.as_str()).unwrap_or("?")
            );
        }
        println!(
            "  task_spawn_locs from reader: {} entries",
            reader.task_spawn_locs.len()
        );
        println!("  task_locs from PollStart: {} entries", task_locs.len());
        // Check: are the unresolved IDs in task_spawn_locs?
        for (id, count) in top_unresolved.iter().take(5) {
            let in_spawn = reader
                .task_spawn_locs
                .contains_key(&dial9_tokio_telemetry::telemetry::TaskId::from_u32(*id));
            println!(
                "    0x{:08x}: in task_spawn_locs={}, in task_locs={}, wakes={}",
                id,
                in_spawn,
                task_locs.contains_key(id),
                count
            );
        }
        let mut by_count: Vec<_> = wakes_by_loc.into_iter().collect();
        by_count.sort_by(|a, b| b.1.cmp(&a.1));
        for (loc, count) in &by_count {
            let name = loc.unwrap_or("<unknown>");
            println!("  {:>8} wakes from {}", count, name);
        }
    }

    if idle_periods.is_empty() {
        println!("No significant idle periods detected with work in queue");
    } else {
        println!(
            "Found {} idle periods with work in queue:",
            idle_periods.len()
        );
        for (worker_id, duration_ns, queue_depth) in idle_periods.iter().take(10) {
            println!(
                "  Worker {} idle for {:.2}ms with {} tasks in global queue",
                worker_id,
                *duration_ns as f64 / 1_000_000.0,
                queue_depth
            );
        }
    }
}
