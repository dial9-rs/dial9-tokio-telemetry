use dial9_tokio_telemetry::telemetry::{
    TraceReader, analyze_trace, detect_idle_workers, print_analysis,
};
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
