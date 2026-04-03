//! Multiple named runtimes sharing a single telemetry session.
//!
//! A common pattern is to run separate runtimes for different workload types
//! (e.g. request handling vs background I/O). This example shows how to
//! attach a second runtime to an existing telemetry session with
//! `build_with_reuse`, so all workers appear in a single trace file with
//! their runtime names in the segment metadata.
//!
//! Usage:
//!   cargo run --example multi_runtime
//!
//! After running, inspect the trace:
//!   cargo run --example analyze_trace -- /tmp/multi_runtime/trace.0.bin

use dial9_tokio_telemetry::telemetry::{RotatingWriter, TracedRuntime};
use std::time::Duration;

fn main() -> std::io::Result<()> {
    let trace_dir = "/tmp/multi_runtime";
    let _ = std::fs::create_dir_all(trace_dir);

    let writer = RotatingWriter::builder()
        .base_path(format!("{trace_dir}/trace.bin"))
        .max_file_size(1024 * 1024)
        .max_total_size(5 * 1024 * 1024)
        .build()?;

    // Primary runtime for request handling.
    let mut builder = tokio::runtime::Builder::new_multi_thread();
    builder.worker_threads(2).enable_all();

    let (main_rt, guard) = TracedRuntime::builder()
        .with_runtime_name("main")
        .with_trace_path(format!("{trace_dir}/trace.bin"))
        .build_and_start(builder, writer)?;

    // Secondary runtime for background I/O, sharing the same trace session.
    let mut io_builder = tokio::runtime::Builder::new_multi_thread();
    io_builder.worker_threads(2).enable_all();

    let io_rt = TracedRuntime::builder()
        .with_runtime_name("io")
        .build_with_reuse(io_builder, &guard)?;

    println!("Running workload on two named runtimes...");

    // Simulate request handling on the main runtime.
    main_rt.block_on(async {
        let mut handles = Vec::new();
        for i in 0..20 {
            handles.push(tokio::spawn(async move {
                // Simulate request processing: some CPU work + async I/O.
                tokio::task::yield_now().await;
                tokio::time::sleep(Duration::from_millis(5)).await;
                tokio::task::yield_now().await;
                println!("  [main] request {i} done");
            }));
        }
        for h in handles {
            h.await.unwrap();
        }
    });

    // Simulate background I/O on the io runtime.
    io_rt.block_on(async {
        let mut handles = Vec::new();
        for i in 0..10 {
            handles.push(tokio::spawn(async move {
                tokio::time::sleep(Duration::from_millis(10)).await;
                tokio::task::yield_now().await;
                println!("  [io]   batch {i} flushed");
            }));
        }
        for h in handles {
            h.await.unwrap();
        }
    });

    println!("All tasks completed.");

    // Drop runtimes before the guard so worker threads flush their buffers.
    drop(main_rt);
    drop(io_rt);
    let _ = guard.graceful_shutdown(Duration::from_secs(5));

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
    println!("\nThe trace viewer will show workers grouped by runtime name (main / io).");

    Ok(())
}
