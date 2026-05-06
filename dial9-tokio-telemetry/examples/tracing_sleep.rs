//! Demonstrates tracing spans across async sleep boundaries.
//! Each span is polled multiple times (producing multiple segments in the viewer).
use dial9_tokio_telemetry::telemetry::{RotatingWriter, TracedRuntime};
use dial9_tokio_telemetry::tracing_layer::Dial9TokioLayer;
use std::time::Duration;
use tracing_subscriber::prelude::*;

#[tracing::instrument]
async fn handle_request(id: u32) {
    inner_work(id).await;
}

#[tracing::instrument]
async fn inner_work(id: u32) {
    for _ in 0..3 {
        tokio::time::sleep(Duration::from_millis(5)).await;
    }
    let _ = id;
}

fn main() {
    let mut builder = tokio::runtime::Builder::new_multi_thread();
    builder.worker_threads(2).enable_all();

    let writer = RotatingWriter::single_file("tracing_sleep_trace.bin").unwrap();
    let (runtime, _guard) = TracedRuntime::builder()
        .with_task_tracking(true)
        .build_and_start(builder, writer)
        .unwrap();

    let subscriber = tracing_subscriber::registry().with(Dial9TokioLayer::new());
    tracing::subscriber::set_global_default(subscriber).expect("failed to set subscriber");

    runtime.block_on(async {
        let tasks: Vec<_> = (0..10).map(|i| tokio::spawn(handle_request(i))).collect();
        for t in tasks {
            let _ = t.await;
        }
    });

    println!("Trace written to tracing_sleep_trace.bin");
}
