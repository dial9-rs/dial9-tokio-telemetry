//! Example: sched events with kernel stack frames.
//!
//! Captures context-switch callchains that include kernel frames, showing
//! exactly where in the kernel the thread was descheduled.
//!
//! Run with:
//!   cargo run --release --features cpu-profiling --example kernel_sched_events
//!
//! Requirements:
//!   - perf_event_paranoid ≤ 1:  sudo sysctl kernel.perf_event_paranoid=1
//!   - For kernel symbol names:  sudo sysctl kernel.kptr_restrict=0
//!     (otherwise kernel frames show as "[kernel] 0x..." addresses)
//!
//! Example output (nanosleep descheduling a tokio worker):
//!
//!   [kernel] schedule
//!   [kernel] schedule_hrtimeout_range_clock
//!   [kernel] do_nanosleep
//!   [kernel] hrtimer_nanosleep
//!   [kernel] common_nsleep_timens
//!   [kernel] __x64_sys_nanosleep
//!   [kernel] do_syscall_64
//!   __GI___nanosleep                              ← libc
//!   std::thread::sleep                            ← userspace
//!   kernel_sched_events::blocking_task::{{closure}}
//!   tokio::runtime::task::core::Core<T,S>::poll
//!   ...
//!   start_thread

use dial9_tokio_telemetry::telemetry::{RotatingWriter, SchedEventConfig, TracedRuntime};
use std::time::Duration;

async fn blocking_task(id: usize) {
    for _ in 0..5 {
        std::thread::sleep(Duration::from_millis(10));
        tokio::task::yield_now().await;
    }
    println!("Task {id} done");
}

fn main() {
    let mut builder = tokio::runtime::Builder::new_multi_thread();
    builder.worker_threads(2).enable_all();

    let writer = RotatingWriter::single_file("kernel_sched_trace.bin").unwrap();
    let (runtime, _guard) = TracedRuntime::builder()
        .with_task_tracking(true)
        .with_sched_events(SchedEventConfig {
            include_kernel: true,
        })
        .with_inline_callframe_symbols(true)
        .build_and_start(builder, writer)
        .unwrap();

    runtime.block_on(async {
        let tasks: Vec<_> = (0..4).map(|i| tokio::spawn(blocking_task(i))).collect();
        for t in tasks {
            let _ = t.await;
        }
    });

    println!("Trace written to kernel_sched_trace.bin");
}
