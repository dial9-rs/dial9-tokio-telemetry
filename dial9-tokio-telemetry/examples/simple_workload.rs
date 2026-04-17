//! Minimal example: add dial9 telemetry to an async app.
//!
//! The `#[dial9_tokio_telemetry::main]` macro replaces `#[tokio::main]`.
//! It builds the Tokio runtime from a config function and spawns the body as
//! an instrumented task so top-level code is visible in traces.
//!
//! Usage:
//!   cargo run --example simple_workload
//!
//! Inspect the trace afterwards:
//!   cargo run --example analyze_trace -- simple_workload_trace.0.bin

use std::time::Duration;

use dial9_tokio_telemetry::config::{Dial9Config, Dial9ConfigBuilder};
use dial9_tokio_telemetry::telemetry::TelemetryHandle;

fn my_config() -> Dial9Config {
    Dial9ConfigBuilder::new(
        "simple_workload_trace.bin",
        64 * 1024 * 1024,
        256 * 1024 * 1024,
    )
    .with_tokio(|t| {
        t.worker_threads(4);
    })
    .with_runtime(|r| r.with_task_tracking(true))
    .build()
}

async fn cpu_work(iterations: u64) -> u64 {
    let mut result = 0u64;
    for i in 0..iterations {
        result = result.wrapping_add(i.wrapping_mul(i));
    }
    result
}

async fn io_simulation() {
    tokio::time::sleep(Duration::from_millis(10)).await;
}

async fn mixed_task(id: usize) {
    for i in 0..10 {
        if i % 3 == 0 {
            io_simulation().await;
        } else {
            cpu_work(100_000).await;
        }
        tokio::task::yield_now().await;
    }
    println!("Task {id} completed");
}

#[dial9_tokio_telemetry::main(config = my_config)]
async fn main() {
    println!("Running workload...");

    let handle = TelemetryHandle::current();
    let tasks: Vec<_> = (0..200).map(|i| handle.spawn(mixed_task(i))).collect();

    for task in tasks {
        let _ = task.await;
    }

    println!("Trace written to simple_workload_trace.*.bin");
}
