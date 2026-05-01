//! Generate a trace with 48 workers for testing the viewer with many lanes.
//!
//! Usage:
//!   cargo run --example many_workers
//!
//! Then open the trace in the viewer:
//!   cargo run -p dial9-viewer -- serve --local-dir .

use std::time::Duration;

use dial9_tokio_telemetry::config::{Dial9Config, Dial9ConfigBuilder};
use dial9_tokio_telemetry::telemetry::TelemetryHandle;

fn my_config() -> Dial9Config {
    Dial9ConfigBuilder::new(
        "many_workers_trace.bin",
        64 * 1024 * 1024,
        256 * 1024 * 1024,
    )
    .with_tokio(|t| {
        t.worker_threads(48);
    })
    .with_runtime(|r| r.with_task_tracking(true))
    .build()
}

#[dial9_tokio_telemetry::main(config = my_config)]
async fn main() {
    println!("Running workload with 48 workers...");

    let handle = TelemetryHandle::current();
    let tasks: Vec<_> = (0..500)
        .map(|i| {
            handle.spawn(async move {
                for _ in 0..5 {
                    tokio::time::sleep(Duration::from_millis(5)).await;
                    // Small CPU work to generate poll events
                    let mut v = 0u64;
                    for j in 0..50_000u64 {
                        v = v.wrapping_add(j.wrapping_mul(j));
                    }
                    std::hint::black_box(v);
                    tokio::task::yield_now().await;
                }
                if i % 100 == 0 {
                    println!("Task {i} done");
                }
            })
        })
        .collect();

    for task in tasks {
        let _ = task.await;
    }

    println!("Trace written to many_workers_trace.*.bin");
}
