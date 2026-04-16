//! Example: using the `#[dial9_tokio_telemetry::main]` macro.
//!
//! The macro replaces `#[tokio::main]` and sets up the traced runtime
//! automatically from a config function — no manual `TracedRuntime` wiring.
//!
//! Run with telemetry enabled:
//! ```sh
//! ENABLE_DIAL9=1 cargo run --example macro_workload
//! ```
//!
//! Run with telemetry disabled (plain tokio runtime):
//! ```sh
//! cargo run --example macro_workload
//! ```

use std::time::Duration;

use dial9_tokio_telemetry::config::{Dial9Config, Dial9ConfigBuilder};
use dial9_tokio_telemetry::telemetry::TelemetryHandle;

/// Configuration function passed to the macro via `config = ...`.
///
/// Returns a [`Dial9Config`] built from either an enabled or disabled builder
/// depending on the `ENABLE_DIAL9` environment variable.
fn my_config() -> Dial9Config {
    if std::env::var("ENABLE_DIAL9").is_err() {
        return Dial9ConfigBuilder::disabled()
            .with_tokio(|t| {
                t.worker_threads(4);
            })
            .build();
    }

    Dial9ConfigBuilder::new(
        "macro_workload_trace.bin",
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
    let telemetry_enabled = TelemetryHandle::try_current().is_some();
    println!(
        "Running macro workload (telemetry {})...",
        if telemetry_enabled {
            "enabled"
        } else {
            "disabled"
        }
    );

    // When telemetry is enabled, use handle.spawn() for wake-event tracking.
    // When disabled, fall back to plain tokio::spawn().
    let tasks: Vec<_> = (0..50)
        .map(|i| match TelemetryHandle::try_current() {
            Some(handle) => handle.spawn(mixed_task(i)),
            None => tokio::spawn(mixed_task(i)),
        })
        .collect();

    for task in tasks {
        let _ = task.await;
    }

    if telemetry_enabled {
        println!("All tasks completed — trace written to macro_workload_trace.*.bin");
    } else {
        println!("All tasks completed — set ENABLE_DIAL9=1 to enable tracing");
    }
}
