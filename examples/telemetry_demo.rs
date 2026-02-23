use dial9_tokio_telemetry::telemetry::{SimpleBinaryWriter, TelemetryRecorder};
use std::time::Duration;

fn main() {
    let mut builder = tokio::runtime::Builder::new_multi_thread();
    builder.worker_threads(4).enable_all();

    let writer = Box::new(SimpleBinaryWriter::new("telemetry_trace.bin").unwrap());
    let recorder = TelemetryRecorder::install(&mut builder, writer);
    let runtime = builder.build().unwrap();

    recorder
        .lock()
        .unwrap()
        .initialize(runtime.handle().clone());

    runtime.block_on(async {
        let _flush = TelemetryRecorder::start_flush_task(recorder.clone(), Duration::from_secs(1));

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

    drop(runtime);
    drop(recorder);
    println!("Trace written to telemetry_trace.bin");
}
