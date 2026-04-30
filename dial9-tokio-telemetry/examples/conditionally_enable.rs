//! Example: conditionally enable dial9 telemetry via an environment variable.
//!
//! A common pattern is to run with telemetry in staging or on-demand in
//! production, while keeping a plain tokio runtime in dev. The `config`
//! function checks `ENABLE_DIAL9` and returns either an enabled or disabled
//! [`Dial9Config`] — the macro handles both cases transparently.
//!
//! Run with telemetry enabled:
//! ```sh
//! ENABLE_DIAL9=1 cargo run --example conditionally_enable
//! ```
//!
//! Run with telemetry disabled (plain tokio runtime):
//! ```sh
//! cargo run --example conditionally_enable
//! ```

use std::time::Duration;

use dial9_tokio_telemetry::Dial9Config;
use dial9_tokio_telemetry::telemetry::TelemetryHandle;

fn my_config() -> Dial9Config {
    Dial9Config::builder()
        .enabled(std::env::var("ENABLE_DIAL9").is_ok())
        .base_path("conditionally_enable_trace.bin")
        .max_file_size(64 * 1024 * 1024)
        .max_total_size(256 * 1024 * 1024)
        .with_tokio(|t| {
            t.worker_threads(4);
        })
        .with_runtime(|r| r.with_task_tracking(true))
        .build_or_disabled()
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
        "Running workload (telemetry {})...",
        if telemetry_enabled {
            "enabled"
        } else {
            "disabled"
        }
    );

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
        println!("All tasks completed — trace written to conditionally_enable_trace.*.bin");
    } else {
        println!("All tasks completed — set ENABLE_DIAL9=1 to enable tracing");
    }
}
