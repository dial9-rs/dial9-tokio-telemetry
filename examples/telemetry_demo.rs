use dial9_tokio_telemetry::telemetry::{SimpleBinaryWriter, TracedRuntime};
use std::time::Duration;

fn main() {
    let mut builder = tokio::runtime::Builder::new_multi_thread();
    builder.worker_threads(4).enable_all();

    let writer = Box::new(SimpleBinaryWriter::new("telemetry_trace.bin").unwrap());
    let (runtime, _guard) = TracedRuntime::builder()
        .build_and_start(builder, writer)
        .unwrap();

    runtime.block_on(async {
        println!("Starting telemetry demo...");

        let tasks: Vec<_> = (0..10)
            .map(|i| {
                tokio::spawn(async move {
                    for j in 0..100 {
                        if j % 10 == 0 {
                            tokio::time::sleep(Duration::from_millis(10)).await;
                        }
                        tokio::task::yield_now().await;
                    }
                    println!("Task {} completed", i);
                })
            })
            .collect();

        for task in tasks {
            task.await.unwrap();
        }

        println!("All tasks completed");
    });

    println!("Trace written to telemetry_trace.bin");
}
