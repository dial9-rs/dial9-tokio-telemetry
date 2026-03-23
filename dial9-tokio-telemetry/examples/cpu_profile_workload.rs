//! Example: traced runtime with CPU profiling enabled.
//!
//! Runs a workload with some CPU-heavy polls, then reads back the trace
//! and prints any CpuSample events found.
//!
//! Run with:
//!   RUSTFLAGS="-C force-frame-pointers=yes" cargo run --release --features cpu-profiling --example cpu_profile_workload
//!
//! You may need:
//!   echo 2 | sudo tee /proc/sys/kernel/perf_event_paranoid

use dial9_tokio_telemetry::telemetry::{
    CpuProfilingConfig, RotatingWriter, TelemetryEvent, TracedRuntime,
};
use std::time::Duration;

#[inline(never)]
fn burn_cpu(iterations: u64) -> u64 {
    let mut result = 0u64;
    for i in 0..iterations {
        result = result.wrapping_add(i.wrapping_mul(i));
    }
    result
}

async fn cpu_heavy_task(id: usize) {
    for _ in 0..5 {
        // This poll will show up as a long poll with CPU samples inside it
        let _ = burn_cpu(5_000_000);
        tokio::task::yield_now().await;
    }
    eprintln!("Task {id} done");
}

fn main() {
    let trace_path = "cpu_profile_trace.bin";

    let mut builder = tokio::runtime::Builder::new_multi_thread();
    builder.worker_threads(4).enable_all();

    let writer = RotatingWriter::single_file(trace_path).unwrap();
    let (runtime, guard) = TracedRuntime::builder()
        .with_task_tracking(true)
        .with_cpu_profiling(CpuProfilingConfig::default())
        .build_and_start(builder, writer)
        .unwrap();

    eprintln!("Running workload with CPU profiling at {} Hz...", 99);
    runtime.block_on(async {
        let tasks: Vec<_> = (0..200).map(|i| tokio::spawn(cpu_heavy_task(i))).collect();
        for task in tasks {
            let _ = task.await;
        }
        // Give the flush thread time to drain samples
        tokio::time::sleep(Duration::from_millis(500)).await;
    });

    drop(runtime);
    drop(guard);

    // Read back and report
    eprintln!("\n=== Reading trace from {trace_path} ===");
    let reader = dial9_tokio_telemetry::analysis_unstable::TraceReader::new(trace_path).unwrap();
    let events = &reader.runtime_events;
    let mut cpu_samples = 0;
    let mut polls = 0;
    let mut samples_by_worker: std::collections::HashMap<u64, usize> =
        std::collections::HashMap::new();

    for event in events {
        match event {
            TelemetryEvent::CpuSample {
                worker_id,
                callchain,
                timestamp_nanos,
                source,
                ..
            } => {
                cpu_samples += 1;
                *samples_by_worker.entry(worker_id.as_u64()).or_default() += 1;
                if cpu_samples <= 10 {
                    eprintln!(
                        "  CpuSample: worker={worker_id} t={timestamp_nanos}ns source={source:?} frames={}",
                        callchain.len()
                    );
                    for (i, addr) in callchain.iter().take(8).enumerate() {
                        eprintln!("    [{i}] {addr:#x}");
                    }
                }
            }
            TelemetryEvent::PollStart { .. } => polls += 1,
            _ => {}
        }
    }

    eprintln!("\nTotal events: {}", events.len());
    eprintln!("Poll starts: {polls}");
    eprintln!("CPU samples: {cpu_samples}");
    for (worker, count) in &samples_by_worker {
        eprintln!("  worker {worker}: {count} samples");
    }
    if cpu_samples == 0 {
        eprintln!("\nNo CPU samples collected! Check:");
        eprintln!("  - perf_event_paranoid: cat /proc/sys/kernel/perf_event_paranoid");
        eprintln!("  - frame pointers: RUSTFLAGS=\"-C force-frame-pointers=yes\"");
    }
}
