use dial9_tokio_telemetry::telemetry::{SimpleBinaryWriter, TracedRuntime};
use std::time::Duration;

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
            cpu_work(100000).await;
        }
        tokio::task::yield_now().await;
    }
    println!("Task {} completed", id);
}

fn main() {
    let mut builder = tokio::runtime::Builder::new_multi_thread();
    builder.worker_threads(4).enable_all();

    let writer = Box::new(SimpleBinaryWriter::new("workload_trace.bin").unwrap());
    let (runtime, _guard) = TracedRuntime::build_and_start(builder, writer).unwrap();

    println!("Running workload...");
    runtime.block_on(async {
        let tasks: Vec<_> = (0..20).map(|i| tokio::spawn(mixed_task(i))).collect();
        for task in tasks {
            let _ = task.await;
        }
    });

    println!("Trace written to workload_trace.bin");
}
