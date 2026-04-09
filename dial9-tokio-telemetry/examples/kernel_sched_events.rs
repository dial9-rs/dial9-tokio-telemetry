//! Example: sched events with kernel stack frames.
//!
//! Captures context-switch callchains that include kernel frames, showing
//! exactly where in the kernel the thread was descheduled. Reads back the
//! trace and prints sample callchains so you can verify your setup.
//!
//! Run with:
//!   cargo run --release --features cpu-profiling --example kernel_sched_events
//!
//! Requirements:
//!   - perf_event_paranoid ≤ 1:  sudo sysctl kernel.perf_event_paranoid=1
//!
//! Example output (nanosleep descheduling a tokio worker):
//!
//!   __schedule                                    ← kernel
//!   schedule
//!   do_nanosleep
//!   hrtimer_nanosleep
//!   __x64_sys_nanosleep
//!   do_syscall_64
//!   entry_SYSCALL_64_after_hwframe
//!   __GI___nanosleep                              ← libc
//!   std::thread::sleep                            ← userspace
//!   kernel_sched_events::blocking_task::{{closure}}
//!   tokio::runtime::task::core::Core<T,S>::poll
//!   ...
//!   start_thread
//!
//! Example output (tokio worker parking on futex):
//!
//!   __schedule                                    ← kernel
//!   schedule
//!   futex_wait_queue_me
//!   futex_wait
//!   do_futex
//!   __x64_sys_futex
//!   do_syscall_64
//!   entry_SYSCALL_64_after_hwframe
//!   syscall                                       ← libc
//!   tokio::..::park::Inner::park_condvar          ← userspace
//!   tokio::..::worker::Context::park_internal
//!   ...
//!   start_thread

use dial9_tokio_telemetry::analysis_unstable::TraceReader;
use dial9_tokio_telemetry::telemetry::{
    CpuSampleSource, RotatingWriter, TelemetryEvent, TracedRuntime, cpu_profile::SchedEventConfig,
};
use std::time::Duration;

async fn blocking_task(id: usize) {
    for _ in 0..5 {
        std::thread::sleep(Duration::from_millis(10));
        tokio::task::yield_now().await;
    }
    eprintln!("Task {id} done");
}

fn main() {
    let trace_dir = "example-traces";
    std::fs::create_dir_all(trace_dir).unwrap();
    let trace_path = format!("{trace_dir}/kernel_sched_trace.bin");

    let mut builder = tokio::runtime::Builder::new_multi_thread();
    builder.worker_threads(2).enable_all();

    let writer = RotatingWriter::single_file(&trace_path).unwrap();
    let (runtime, guard) = TracedRuntime::builder()
        .with_task_tracking(true)
        .with_sched_events(SchedEventConfig {
            include_kernel: true,
        })
        .build_and_start(builder, writer)
        .unwrap();

    runtime.block_on(async {
        let tasks: Vec<_> = (0..4).map(|i| tokio::spawn(blocking_task(i))).collect();
        for t in tasks {
            let _ = t.await;
        }
    });

    drop(runtime);
    drop(guard);

    // Read back and print callchains
    eprintln!("\n=== Reading trace from {trace_path} ===");
    let reader = TraceReader::new(&trace_path).unwrap();
    let events = &reader.runtime_events;

    let mut printed = 0;
    let mut total_samples = 0;

    for event in events {
        if let TelemetryEvent::CpuSample {
            worker_id,
            source,
            callchain,
            ..
        } = event
        {
            if *source != CpuSampleSource::SchedEvent {
                continue;
            }
            total_samples += 1;
            if printed < 3 {
                printed += 1;
                eprintln!("\n--- SchedEvent sample #{printed} (worker {worker_id}) ---");
                for addr in callchain {
                    eprintln!("  {addr:#x}");
                }
            }
        }
    }

    eprintln!("\nTotal sched event samples: {total_samples}");
    if total_samples == 0 {
        eprintln!("No samples! Check: sudo sysctl kernel.perf_event_paranoid=1");
    }
}
