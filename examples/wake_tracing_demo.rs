//! Demo: one producer → N workers → one collector, all on a single traced runtime.
//! Wake events should clearly show spawn locations since all tasks are local.
//!
//! Usage: cargo run --example wake_tracing_demo

use dial9_tokio_telemetry::telemetry::{SimpleBinaryWriter, TracedRuntime};
use std::sync::Arc;
use tokio::sync::{Mutex, mpsc};

const NUM_WORKERS: usize = 4;
const NUM_ITEMS: usize = 1000;
const TRACE_FILE: &str = "wake_tracing_demo.bin";

fn main() -> std::io::Result<()> {
    let writer = Box::new(SimpleBinaryWriter::new(TRACE_FILE)?);
    let mut builder = tokio::runtime::Builder::new_multi_thread();
    builder.worker_threads(8).enable_all();

    let (rt, guard) = TracedRuntime::builder()
        .with_task_tracking(true)
        .build_and_start(builder, writer)?;

    rt.block_on(async {
        println!("{:?}", tokio::task::try_id());
        let (work_tx, work_rx) = mpsc::channel::<u64>(16);
        let work_rx = Arc::new(Mutex::new(work_rx));
        let (result_tx, mut result_rx) = mpsc::channel::<u64>(16);

        // Producer: pushes work items
        guard.spawn(async move {
            for i in 0..NUM_ITEMS as u64 {
                work_tx.send(i).await.unwrap();
            }
        });

        // Workers: read from work channel, write to result channel
        for _ in 0..NUM_WORKERS {
            let rx = work_rx.clone();
            let tx = result_tx.clone();
            guard.spawn(async move {
                loop {
                    let item = {
                        let mut rx = rx.lock().await;
                        rx.recv().await
                    };
                    match item {
                        Some(val) => {
                            tokio::task::yield_now().await;
                            tx.send(val * 2).await.unwrap();
                        }
                        None => break,
                    }
                }
            });
        }
        drop(result_tx);

        // Collector: reads results
        let collector = guard.spawn(async move {
            let mut count = 0u64;
            while let Some(_val) = result_rx.recv().await {
                count += 1;
            }
            count
        });

        let count = collector.await.unwrap();
        println!("Collected {count} results");
    });

    drop(rt);
    drop(guard);
    println!("Trace written to {TRACE_FILE}");
    println!("Analyze: cargo run --example analyze_trace -- {TRACE_FILE}");
    Ok(())
}
