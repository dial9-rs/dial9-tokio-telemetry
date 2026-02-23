//! Convert a TOKIOTRC binary trace to JSONL (one JSON object per line).
//!
//! Usage:
//!   cargo run --example trace_to_jsonl -- <input.bin> [output.jsonl]
//!
//! If output is omitted, writes to stdout.

use dial9_tokio_telemetry::telemetry::{EventType, TraceReader};
use std::io::{BufWriter, Write};

fn event_type_str(et: EventType) -> &'static str {
    match et {
        EventType::PollStart => "PollStart",
        EventType::PollEnd => "PollEnd",
        EventType::WorkerPark => "WorkerPark",
        EventType::WorkerUnpark => "WorkerUnpark",
        EventType::QueueSample => "QueueSample",
    }
}

fn main() -> std::io::Result<()> {
    let args: Vec<String> = std::env::args().collect();
    if args.len() < 2 {
        eprintln!("usage: trace_to_jsonl <input.bin> [output.jsonl]");
        std::process::exit(1);
    }

    let mut reader = TraceReader::new(&args[1])?;
    let (magic, version) = reader.read_header()?;
    if magic != "TOKIOTRC" {
        eprintln!("not a TOKIOTRC file (got: {magic})");
        std::process::exit(1);
    }
    eprintln!("TOKIOTRC v{version}, converting...");

    let out: Box<dyn Write> = if let Some(path) = args.get(2) {
        Box::new(std::fs::File::create(path)?)
    } else {
        Box::new(std::io::stdout().lock())
    };
    let mut w = BufWriter::new(out);

    let mut count = 0u64;
    while let Some(e) = reader.read_event()? {
        let cpu_field = match e.event_type {
            EventType::WorkerPark | EventType::WorkerUnpark => {
                format!(",\"cpu_ns\":{}", e.metrics.cpu_time_nanos)
            }
            _ => String::new(),
        };
        let sched_field = match e.event_type {
            EventType::WorkerUnpark => {
                format!(",\"sched_wait_ns\":{}", e.metrics.sched_wait_delta_nanos)
            }
            _ => String::new(),
        };
        write!(
            w,
            "{{\"event\":\"{}\",\"timestamp_ns\":{},\"worker\":{},\"global_q\":{},\"local_q\":{}{}{}}}\n",
            event_type_str(e.event_type),
            e.metrics.timestamp_nanos,
            e.metrics.worker_id,
            e.metrics.global_queue_depth,
            e.metrics.worker_local_queue_depth,
            cpu_field,
            sched_field,
        )?;
        count += 1;
    }
    w.flush()?;
    eprintln!("{count} events written");
    Ok(())
}
