//! Benchmark comparing direct encode vs thread-local encode + transcode.
//!
//! Measures the end-to-end cost of:
//! - Direct: encode events directly through a single Encoder<Vec<u8>>
//! - Thread-local: encode through a thread-local Encoder, reset(), then
//!   transcode into a central Encoder<Vec<u8>>
//!
//! Usage:
//!   cargo bench --bench threadlocal_encode

use criterion::{BenchmarkId, Criterion, criterion_group, criterion_main};
use dial9_tokio_telemetry::telemetry::format::{
    PollEndEvent, PollStartEvent, WorkerParkEvent, WorkerUnparkEvent,
};
use dial9_tokio_telemetry::telemetry::{TaskId, WorkerId};
use dial9_trace_format::encoder::Encoder;
use dial9_trace_format::transcoder::transcode;

fn make_batch(worker: usize) -> Vec<(u64, WorkerId, TaskId)> {
    let wid = WorkerId::from(worker);
    let task = TaskId::from_u32(1);
    let mut events = Vec::with_capacity(350);

    for cycle in 0..3u64 {
        let base = cycle * 10_000;
        events.push((base + 100, wid, task));
        for i in 0..170u64 {
            events.push((base + 200 + i * 10, wid, task));
        }
        events.push((base + 3000, wid, task));
    }
    events
}

fn bench_direct_encode(c: &mut Criterion) {
    let mut group = c.benchmark_group("direct_encode");

    for num_batches in [1, 10, 100] {
        let batches: Vec<_> = (0..num_batches).map(|i| make_batch(i % 8)).collect();
        let total_events: usize = batches.iter().map(|b| b.len() * 2).sum();
        group.throughput(criterion::Throughput::Elements(total_events as u64));

        group.bench_with_input(
            BenchmarkId::new("batches", num_batches),
            &batches,
            |b, batches| {
                b.iter(|| {
                    let mut encoder = Encoder::new();
                    let spawn_loc = encoder.intern_string("test").unwrap();
                    for batch in batches {
                        for &(ts, wid, task) in batch {
                            encoder
                                .write(&PollStartEvent {
                                    timestamp_ns: ts,
                                    worker_id: wid,
                                    local_queue: 3,
                                    task_id: task,
                                    spawn_loc,
                                })
                                .unwrap();
                            encoder
                                .write(&PollEndEvent {
                                    timestamp_ns: ts + 5,
                                    worker_id: wid,
                                })
                                .unwrap();
                        }
                        encoder
                            .write(&WorkerParkEvent {
                                timestamp_ns: batch.last().unwrap().0 + 100,
                                worker_id: batch[0].1,
                                local_queue: 0,
                                cpu_time_ns: 600_000,
                            })
                            .unwrap();
                        encoder
                            .write(&WorkerUnparkEvent {
                                timestamp_ns: batch[0].0,
                                worker_id: batch[0].1,
                                local_queue: 5,
                                cpu_time_ns: 500_000,
                                sched_wait_ns: 1_000,
                            })
                            .unwrap();
                    }
                    encoder.finish()
                });
            },
        );
    }
    group.finish();
}

fn bench_threadlocal_transcode(c: &mut Criterion) {
    let mut group = c.benchmark_group("threadlocal_transcode");

    for num_batches in [1, 10, 100] {
        let batches: Vec<_> = (0..num_batches).map(|i| make_batch(i % 8)).collect();
        let total_events: usize = batches.iter().map(|b| b.len() * 2).sum();
        group.throughput(criterion::Throughput::Elements(total_events as u64));

        group.bench_with_input(
            BenchmarkId::new("batches", num_batches),
            &batches,
            |b, batches| {
                b.iter(|| {
                    let mut central = Encoder::new();
                    for batch in batches {
                        let mut local = Encoder::new();
                        let spawn_loc = local.intern_string("test").unwrap();
                        for &(ts, wid, task) in batch {
                            local
                                .write(&PollStartEvent {
                                    timestamp_ns: ts,
                                    worker_id: wid,
                                    local_queue: 3,
                                    task_id: task,
                                    spawn_loc,
                                })
                                .unwrap();
                            local
                                .write(&PollEndEvent {
                                    timestamp_ns: ts + 5,
                                    worker_id: wid,
                                })
                                .unwrap();
                        }
                        local
                            .write(&WorkerParkEvent {
                                timestamp_ns: batch.last().unwrap().0 + 100,
                                worker_id: batch[0].1,
                                local_queue: 0,
                                cpu_time_ns: 600_000,
                            })
                            .unwrap();
                        local
                            .write(&WorkerUnparkEvent {
                                timestamp_ns: batch[0].0,
                                worker_id: batch[0].1,
                                local_queue: 5,
                                cpu_time_ns: 500_000,
                                sched_wait_ns: 1_000,
                            })
                            .unwrap();

                        let bytes = local.reset();
                        transcode(&bytes, &mut central).unwrap();
                    }
                    central.finish()
                });
            },
        );
    }
    group.finish();
}

criterion_group!(benches, bench_direct_encode, bench_threadlocal_transcode);
criterion_main!(benches);
