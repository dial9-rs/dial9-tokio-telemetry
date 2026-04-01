//! Benchmark for the writer encode path: RawEvent → RotatingWriter → Encoder.
//!
//! This exercises the full `write_resolved` path including event conversion,
//! string interning, and varint encoding. Writes to `/dev/null` to isolate
//! encoding cost from disk I/O.
//!
//! Usage:
//!   cargo bench --bench writer_encode

use criterion::{BenchmarkId, Criterion, criterion_group, criterion_main};
use dial9_tokio_telemetry::telemetry::Batch;
use dial9_tokio_telemetry::telemetry::format::{
    PollEndEvent, PollStartEvent, TaskSpawnEvent, WakeEventEvent, WorkerParkEvent,
    WorkerUnparkEvent,
};
use dial9_tokio_telemetry::telemetry::{RotatingWriter, TaskId, TraceWriter, WorkerId};
use dial9_trace_format::encoder::Encoder;

/// Build a realistic batch simulating a worker thread's activity.
///
/// A typical worker cycle is: unpark → (poll_start, poll_end) × N → park.
/// We simulate ~170 polls per batch (340 events) plus park/unpark and a few
/// spawns and wakes, totalling ~350 events. The batch is repeated ~3× to fill
/// close to 1024 events.
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

fn bench_writer_encode(c: &mut Criterion) {
    let mut group = c.benchmark_group("writer_encode");

    for num_batches in [1, 10, 100] {
        let batches: Vec<_> = (0..num_batches)
            .map(|i| make_encoded_batch(i % 8))
            .collect();
        let total_events: usize = num_batches * 1024; // approximate
        group.throughput(criterion::Throughput::Elements(total_events as u64));

        group.bench_with_input(
            BenchmarkId::new("batches", num_batches),
            &batches,
            |b, batches| {
                let mut writer = RotatingWriter::single_file("/dev/null").unwrap();
                b.iter(|| {
                    for batch in batches {
                        writer.write_encoded_batch(batch).unwrap();
                    }
                    writer.flush().unwrap();
                });
            },
        );
    }
    group.finish();
}

criterion_group!(benches, bench_writer_encode);
criterion_main!(benches);
