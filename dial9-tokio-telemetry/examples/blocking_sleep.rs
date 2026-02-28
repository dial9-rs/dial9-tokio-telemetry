use dial9_tokio_telemetry::telemetry::{SimpleBinaryWriter, TracedRuntime};
use std::time::Duration;

async fn blocking_task(id: usize) {
    for _ in 0..5 {
        // This blocks the worker thread — should show up as a sched event
        std::thread::sleep(Duration::from_millis(10));
        tokio::task::yield_now().await;
    }
    println!("Task {} done", id);
}

fn main() {
    let mut builder = tokio::runtime::Builder::new_multi_thread();
    builder.worker_threads(2).enable_all();

    let writer = Box::new(SimpleBinaryWriter::new("blocking_sleep_trace.bin").unwrap());
    let (runtime, _guard) = TracedRuntime::builder()
        .with_task_tracking(true)
        .with_cpu_profiling(Default::default())
        .with_inline_callframe_symbols(true)
        .with_sched_events(Default::default())
        .build_and_start(builder, writer)
        .unwrap();

    runtime.block_on(async {
        let tasks: Vec<_> = (0..4).map(|i| tokio::spawn(blocking_task(i))).collect();
        for t in tasks {
            let _ = t.await;
        }
    });

    println!("Trace written to blocking_sleep_trace.bin");
}
