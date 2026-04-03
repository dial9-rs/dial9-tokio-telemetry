//! Convert a TOKIOTRC binary trace to JSONL (one JSON object per line).
//!
//! Usage:
//!   cargo run --example trace_to_jsonl -- <input.bin> [output.jsonl]
//!
//! If output is omitted, writes to stdout.

use dial9_tokio_telemetry::analysis_unstable::TraceReader;
use std::io::{BufWriter, Write};

fn main() -> std::io::Result<()> {
    let args: Vec<String> = std::env::args().collect();
    if args.len() < 2 {
        eprintln!("usage: trace_to_jsonl <input.bin> [output.jsonl]");
        std::process::exit(1);
    }

    let reader = TraceReader::new(&args[1])?;
    eprintln!("converting...");

    let out: Box<dyn Write> = if let Some(path) = args.get(2) {
        Box::new(std::fs::File::create(path)?)
    } else {
        Box::new(std::io::stdout().lock())
    };
    let mut w = BufWriter::new(out);

    let mut count = 0u64;
    for e in &reader.all_events {
        serde_json::to_writer(&mut w, &e).map_err(std::io::Error::other)?;
        w.write_all(b"\n")?;
        count += 1;
    }
    w.flush()?;
    eprintln!("{count} events written");
    Ok(())
}
