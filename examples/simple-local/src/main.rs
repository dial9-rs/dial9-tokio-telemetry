use dial9_tokio_telemetry::config::{Dial9Config, Dial9ConfigBuilder};
use dial9_tokio_telemetry::telemetry::TelemetryHandle;
use std::time::Duration;

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

fn my_config() -> Dial9Config {
    let trace_path = format!("{}/trace.bin", TRACE_DIR);
    Dial9ConfigBuilder::new(
        &trace_path,
        10_000_000, // 10MB per file
        50_000_000, // 50MB total
    )
    .with_runtime(|r| r.with_task_tracking(true))
    .with_tokio(|t| {
        t.worker_threads(2);
    })
    .build()
}

#[dial9_tokio_telemetry::main(config = my_config)]
async fn main() {
    let handle = TelemetryHandle::current();
    let mut handles = vec![];

    for _ in 0..100 {
        handles.push(handle.spawn(do_some_work()));
        tokio::time::sleep(Duration::from_millis(1)).await;
    }

    for h in handles {
        h.await.unwrap();
    }

    let trace_path = format!("{}/trace.bin", TRACE_DIR);
    println!("\n✓ Trace files written to: {}", trace_path);
    println!(
        "  You can view them with: cargo run --package dial9-viewer -- --local-dir {}",
        TRACE_DIR
    );
    println!("  Then open http://localhost:3000 in your browser");
}
