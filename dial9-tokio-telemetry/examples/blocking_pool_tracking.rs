//! Example demonstrating blocking pool tracking.
//!
//! Spawns work on tokio's blocking pool via `spawn_blocking`, then writes a
//! trace. Blocking pool samples should appear with `worker_id = 254`
//! (BLOCKING_WORKER) instead of 255 (UNKNOWN_WORKER).
//!
//! Usage:
//!   cargo run --release --features cpu-profiling --example blocking_pool_tracking

use dial9_tokio_telemetry::telemetry::cpu_profile::CpuProfilingConfig;
use dial9_tokio_telemetry::telemetry::{RotatingWriter, TracedRuntime};
use std::time::Duration;

fn burn_cpu(duration: Duration) {
    let start = std::time::Instant::now();
    let mut x: u64 = 1;
    while start.elapsed() < duration {
        for _ in 0..1000 {
            x = x.wrapping_mul(6364136223846793005).wrapping_add(1);
        }
        std::hint::black_box(x);
    }
}

fn main() {
    let mut builder = tokio::runtime::Builder::new_multi_thread();
    builder.worker_threads(2).enable_all();

    let writer = RotatingWriter::single_file("blocking_pool_trace.bin").unwrap();
    let (runtime, _guard) = TracedRuntime::builder()
        .with_task_tracking(true)
        .with_cpu_profiling(CpuProfilingConfig {
            frequency_hz: 999,
            ..Default::default()
        })
        .build_and_start(builder, writer)
        .unwrap();

    runtime.block_on(async {
        // Spawn CPU-intensive work on the blocking pool
        let handles: Vec<_> = (0..3)
            .map(|i| {
                tokio::task::spawn_blocking(move || {
                    eprintln!("blocking task {i} starting");
                    burn_cpu(Duration::from_millis(500));
                    eprintln!("blocking task {i} done");
                })
            })
            .collect();

        for h in handles {
            h.await.unwrap();
        }

        // Let flush cycle capture everything
        tokio::time::sleep(Duration::from_millis(500)).await;
    });

    eprintln!("Trace written to blocking_pool_trace.bin");
}
