//! Convert a TOKIOTRC binary trace to JSONL (one JSON object per line).
//!
//! Usage:
//!   cargo run --example trace_to_jsonl -- <input.bin> [output.jsonl]
//!
//! If output is omitted, writes to stdout.

use dial9_tokio_telemetry::telemetry::TraceReader;
use serde_json;
use std::io::{BufWriter, Write};

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
        serde_json::to_writer(&mut w, &e)
            .map_err(|e| std::io::Error::new(std::io::ErrorKind::Other, e))?;
        w.write_all(b"\n")?;
        count += 1;
    }
    w.flush()?;
    eprintln!("{count} events written");
    Ok(())
}
