//! Example: using the `#[dial9_tokio_telemetry::main]` macro.
//!
//! The macro replaces `#[tokio::main]` and sets up the traced runtime
//! automatically from a config function — no manual `TracedRuntime` wiring.
//!
//! Run with:
//! ```sh
//! cargo run --example macro_workload
//! ```

use std::time::Duration;

use dial9_tokio_telemetry::config::Dial9Config;
use dial9_tokio_telemetry::telemetry::TelemetryHandle;

/// Configuration function passed to the macro via `config = ...`.
/// Must be a zero-argument function returning [`Dial9Config`].
fn my_config() -> Dial9Config {
    Dial9Config::new(
        "macro_workload_trace.bin",
        64 * 1024 * 1024,
        256 * 1024 * 1024,
    )
    .with_tokio(|t| {
        t.worker_threads(4);
    })
    .with_runtime(|r| r.with_task_tracking(true))
}

async fn cpu_work(iterations: u64) -> u64 {
    let mut result = 0u64;
    for i in 0..iterations {
        result = result.wrapping_add(i.wrapping_mul(i));
    }
    result
}

async fn mixed_task(id: usize) {
    for i in 0..10 {
        if i % 3 == 0 {
            tokio::time::sleep(Duration::from_millis(10)).await;
        } else {
            cpu_work(100_000).await;
        }
        tokio::task::yield_now().await;
    }
    println!("Task {id} completed");
}

#[dial9_tokio_telemetry::main(config = my_config)]
async fn main() {
    println!("Running macro workload...");

    // The macro installs `TelemetryHandle::current()` on every runtime thread,
    // so you can grab it anywhere to spawn instrumented sub-tasks.
    let handle = TelemetryHandle::current();

    let tasks: Vec<_> = (0..50).map(|i| handle.spawn(mixed_task(i))).collect();

    for task in tasks {
        let _ = task.await;
    }

    println!("All tasks completed — trace written to macro_workload_trace.*.bin");
}
