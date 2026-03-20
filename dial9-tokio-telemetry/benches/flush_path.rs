//! Benchmark for the writer encode path: RawEvent → RotatingWriter → Encoder.
//!
//! This exercises the full `write_resolved` path including event conversion,
//! string interning, and varint encoding. Writes to `/dev/null` to isolate
//! encoding cost from disk I/O.
//!
//! Usage:
//!   cargo bench --bench flush_path

use criterion::{BenchmarkId, Criterion, criterion_group, criterion_main};
use dial9_tokio_telemetry::telemetry::events::RawEvent;
use dial9_tokio_telemetry::telemetry::{RotatingWriter, TaskId, TraceWriter, WorkerId};

/// Build a realistic batch simulating a worker thread's activity.
///
/// A typical worker cycle is: unpark → (poll_start, poll_end) × N → park.
/// We simulate ~170 polls per batch (340 events) plus park/unpark and a few
/// spawns and wakes, totalling ~350 events. The batch is repeated ~3× to fill
/// close to 1024 events.
fn make_batch(worker: usize) -> Vec<RawEvent> {
    let wid = WorkerId::from(worker);
    let task = TaskId::from_u32(1);
    let loc = std::panic::Location::caller();
    let mut events = Vec::with_capacity(1024);

    for cycle in 0..3u64 {
        let base = cycle * 10_000;
        events.push(RawEvent::WorkerUnpark {
            timestamp_nanos: base + 100,
            worker_id: wid,
            worker_local_queue_depth: 5,
            cpu_time_nanos: 500_000,
            sched_wait_delta_nanos: 1_000,
        });

        for i in 0..170u64 {
            events.push(RawEvent::PollStart {
                timestamp_nanos: base + 200 + i * 10,
                worker_id: wid,
                worker_local_queue_depth: 3,
                task_id: task,
                location: loc,
            });
            events.push(RawEvent::PollEnd {
                timestamp_nanos: base + 205 + i * 10,
                worker_id: wid,
            });
        }

        for _ in 0..3 {
            events.push(RawEvent::TaskSpawn {
                timestamp_nanos: base + 2000,
                task_id: task,
                location: loc,
            });
        }
        for _ in 0..5 {
            events.push(RawEvent::WakeEvent {
                timestamp_nanos: base + 2500,
                waker_task_id: task,
                woken_task_id: task,
                target_worker: worker as u8,
            });
        }

        events.push(RawEvent::WorkerPark {
            timestamp_nanos: base + 3000,
            worker_id: wid,
            worker_local_queue_depth: 0,
            cpu_time_nanos: 600_000,
        });
    }

    events
}

fn bench_writer_encode(c: &mut Criterion) {
    let mut group = c.benchmark_group("writer_encode");

    for num_batches in [1, 10, 100] {
        let batches: Vec<_> = (0..num_batches).map(|i| make_batch(i % 8)).collect();
        let total_events: usize = batches.iter().map(|b| b.len()).sum();
        group.throughput(criterion::Throughput::Elements(total_events as u64));

        group.bench_with_input(
            BenchmarkId::new("batches", num_batches),
            &batches,
            |b, batches| {
                b.iter(|| {
                    let mut writer = RotatingWriter::single_file("/dev/null").unwrap();
                    for batch in batches {
                        writer.write_event_batch(batch).unwrap();
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
