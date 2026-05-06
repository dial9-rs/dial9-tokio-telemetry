//! iai-callgrind benchmark for writer-side encoded-batch ingestion.
//!
//! Measures `RotatingWriter::write_encoded_batch` on pre-encoded `Batch`
//! payloads so we can track writer-path regressions separately from encoder
//! regressions. Uses a temporary file-backed writer (same path as production)
//! and flushes once per run.
//!
//! Usage:
//!   cargo bench --package dial9-tokio-telemetry --bench writer_write_encoded_iai

use dial9_tokio_telemetry::telemetry::{
    Batch, PollEndEvent, PollStartEvent, RotatingWriter, TaskId, TaskSpawnEvent, TraceWriter,
    WakeEventEvent, WorkerId, WorkerParkEvent, WorkerUnparkEvent,
};
use dial9_trace_format::encoder::Encoder;
use iai_callgrind::{library_benchmark, library_benchmark_group, main};
use std::hint::black_box;
use tempfile::TempDir;

fn make_encoded_batch(worker: usize) -> Batch {
    let wid = WorkerId::from(worker);
    let task = TaskId::from_u32(1);
    let mut enc = Encoder::new();
    let loc = enc.intern_string_infallible("src/main.rs:42");

    for cycle in 0..3u64 {
        let base = cycle * 10_000;
        enc.write_infallible(&WorkerUnparkEvent {
            timestamp_ns: base + 100,
            worker_id: wid,
            local_queue: 5,
            cpu_time_ns: 500_000,
            sched_wait_ns: 1_000,
        });

        for i in 0..170u64 {
            enc.write_infallible(&PollStartEvent {
                timestamp_ns: base + 200 + i * 10,
                worker_id: wid,
                local_queue: 3,
                task_id: task,
                spawn_loc: loc,
            });
            enc.write_infallible(&PollEndEvent {
                timestamp_ns: base + 205 + i * 10,
                worker_id: wid,
            });
        }

        for _ in 0..3 {
            enc.write_infallible(&TaskSpawnEvent {
                timestamp_ns: base + 2000,
                task_id: task,
                spawn_loc: loc,
                instrumented: true,
            });
        }
        for _ in 0..5 {
            enc.write_infallible(&WakeEventEvent {
                timestamp_ns: base + 2500,
                waker_task_id: task,
                woken_task_id: task,
                target_worker: worker as u8,
            });
        }

        enc.write_infallible(&WorkerParkEvent {
            timestamp_ns: base + 3000,
            worker_id: wid,
            local_queue: 0,
            cpu_time_ns: 600_000,
        });
    }

    Batch::new(enc.reset_to_infallible(Vec::new()), 1024)
}

fn batches(num_batches: usize) -> Vec<Batch> {
    (0..num_batches)
        .map(|i| make_encoded_batch(i % 8))
        .collect()
}

fn write_encoded(batches: Vec<Batch>) -> usize {
    let tmp = TempDir::new().unwrap();
    let mut writer = RotatingWriter::single_file(tmp.path().join("trace")).unwrap();
    let mut total_bytes = 0usize;

    for batch in &batches {
        total_bytes += batch.encoded_bytes().len();
        writer.write_encoded_batch(batch).unwrap();
    }

    writer.flush().unwrap();
    total_bytes
}

#[library_benchmark]
#[bench::batches_1(batches(1))]
#[bench::batches_10(batches(10))]
#[bench::batches_100(batches(100))]
fn writer_write_encoded(batches: Vec<Batch>) -> usize {
    black_box(write_encoded(black_box(batches)))
}

library_benchmark_group!(name = writer_group; benchmarks = writer_write_encoded);
main!(library_benchmark_groups = writer_group);
