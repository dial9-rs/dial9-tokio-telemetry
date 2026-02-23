//! Telemetry example using `RotatingWriter` for bounded disk usage.
//!
//! This demonstrates how to collect runtime telemetry with automatic file
//! rotation so that trace files never exceed a configured total size on disk.
//!
//! Usage:
//!   cargo run --example telemetry_rotating
//!
//! After running, inspect the trace files:
//!   cargo run --example analyze_trace -- /tmp/telemetry_rotating/trace.0.bin

use dial9_tokio_telemetry::telemetry::{RotatingWriter, TracedRuntime};
use std::time::Duration;

fn main() -> std::io::Result<()> {
    let trace_dir = "/tmp/telemetry_rotating";

    // RotatingWriter rotates to a new file when the current file exceeds
    // `max_file_size`, and deletes the oldest files when total disk usage
    // exceeds `max_total_size`.
    //
    // Files are written as `trace.0.bin`, `trace.1.bin`, etc.
    let writer = RotatingWriter::new(
        format!("{trace_dir}/trace.bin"),
        1024 * 1024,     // rotate after 1 MiB per file
        5 * 1024 * 1024, // keep at most 5 MiB of trace data on disk
    )?;

    // TracedRuntime::build installs telemetry hooks on the tokio runtime
    // builder and returns both the runtime and a guard that manages the
    // background flush/sampler thread.
    let mut builder = tokio::runtime::Builder::new_multi_thread();
    builder.worker_threads(4).enable_all();

    let (runtime, _guard) = TracedRuntime::build_and_start(builder, Box::new(writer))?;

    runtime.block_on(async {
        println!("Starting rotating-writer telemetry demo...");

        // Spawn a batch of tasks that do a mix of yielding and sleeping to
        // generate a realistic stream of poll/park/unpark events.
        let mut handles = Vec::new();
        for i in 0..20 {
            handles.push(tokio::spawn(async move {
                for j in 0..200 {
                    if j % 20 == 0 {
                        tokio::time::sleep(Duration::from_millis(5)).await;
                    }
                    tokio::task::yield_now().await;
                }
                println!("Task {i} completed");
            }));
        }

        for h in handles {
            h.await.unwrap();
        }

        println!("All tasks completed");
    });

    // Dropping the runtime and guard flushes remaining events.
    drop(runtime);
    drop(_guard);

    // List the trace files that were written.
    println!("\nTrace files in {trace_dir}/:");
    for entry in std::fs::read_dir(trace_dir)? {
        let entry = entry?;
        let meta = entry.metadata()?;
        println!(
            "  {} ({} bytes)",
            entry.file_name().to_string_lossy(),
            meta.len()
        );
    }

    println!("\nAnalyze a trace with:");
    println!("  cargo run --example analyze_trace -- {trace_dir}/trace.0.bin");

    Ok(())
}
