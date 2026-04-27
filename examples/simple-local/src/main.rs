use dial9_tokio_telemetry::telemetry::{RotatingWriter, TelemetryHandle, TracedRuntime};
use std::time::Duration;
use tokio::runtime::Builder;

const TRACE_DIR: &str = "/tmp/simple-local-traces";

fn fibonacci_recursive(n: u32) -> u32 {
    match n {
        0 => 0,
        1 => 1,
        _ => fibonacci_recursive(n - 1) + fibonacci_recursive(n - 2),
    }
}

async fn do_some_work() {
    // do some work here
    fibonacci_recursive(25);
}

fn main() -> std::io::Result<()> {
    // Configure the trace writer
    let trace_path = format!("{}/trace.bin", TRACE_DIR);
    let writer = RotatingWriter::builder()
        .base_path(&trace_path)
        .max_file_size(10_000_000) // 10MB per file
        .max_total_size(50_000_000) // 50MB total
        .segment_metadata(vec![
            ("service".into(), "simple-local".into()),
            ("example".into(), "basic".into()),
        ])
        .build()?;

    // Build the traced runtime
    let mut builder = Builder::new_multi_thread();
    builder.worker_threads(2).enable_all();

    let traced_builder = TracedRuntime::builder()
        .with_trace_path(&trace_path)
        .with_task_tracking(true);

    let (runtime, guard) = traced_builder.build(builder, writer)?;
    guard.enable();
    let handle = guard.handle();

    // Run the async code
    runtime.block_on(async {
        handle
            .spawn(async move {
                let telemetry_handle = TelemetryHandle::current();
                let mut handles = vec![];

                // Run some concurrent work
                for _ in 0..100 {
                    handles.push(telemetry_handle.spawn(do_some_work()));
                    tokio::time::sleep(Duration::from_millis(1)).await;
                }

                // Wait for all tasks to complete
                for handle in handles {
                    handle.await.unwrap()
                }
            })
            .await
            .unwrap();
    });

    // Clean shutdown
    drop(runtime);

    guard.graceful_shutdown(Duration::from_secs(1))?;

    println!("\n✓ Trace files written to: {}", trace_path);
    println!(
        "  You can view them with: cargo run --package dial9-viewer -- --local-dir {}",
        TRACE_DIR
    );
    println!("  Then open http://localhost:3000 in your browser");

    Ok(())
}
