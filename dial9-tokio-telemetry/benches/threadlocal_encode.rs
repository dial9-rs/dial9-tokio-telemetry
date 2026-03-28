//! Benchmark comparing direct encode vs thread-local encode + raw-copy.
//!
//! Measures the end-to-end cost of:
//! - Direct: encode events directly through a single Encoder<Vec<u8>>
//! - Thread-local: encode through a thread-local Encoder, reset(), then
//!   raw-copy into a central Vec<u8> (reset-frame pattern)
//!
//! Usage:
//!   cargo bench --bench threadlocal_encode

use criterion::{BenchmarkId, Criterion, criterion_group, criterion_main};
use dial9_tokio_telemetry::telemetry::format::{
    PollEndEvent, PollStartEvent, WorkerParkEvent, WorkerUnparkEvent,
};
use dial9_tokio_telemetry::telemetry::{TaskId, WorkerId};
use dial9_trace_format::encoder::Encoder;

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

fn encode_batch(encoder: &mut Encoder<Vec<u8>>, batch: &[(u64, WorkerId, TaskId)]) {
    let spawn_loc = encoder.intern_string_infallible("test");
    for &(ts, wid, task) in batch {
        encoder.write_infallible(&PollStartEvent {
            timestamp_ns: ts,
            worker_id: wid,
            local_queue: 3,
            task_id: task,
            spawn_loc,
        });
        encoder.write_infallible(&PollEndEvent {
            timestamp_ns: ts + 5,
            worker_id: wid,
        });
    }
    encoder.write_infallible(&WorkerParkEvent {
        timestamp_ns: batch.last().unwrap().0 + 100,
        worker_id: batch[0].1,
        local_queue: 0,
        cpu_time_ns: 600_000,
    });
    encoder.write_infallible(&WorkerUnparkEvent {
        timestamp_ns: batch[0].0,
        worker_id: batch[0].1,
        local_queue: 5,
        cpu_time_ns: 500_000,
        sched_wait_ns: 1_000,
    });
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
                    for batch in batches {
                        encode_batch(&mut encoder, batch);
                    }
                    encoder.finish()
                });
            },
        );
    }
    group.finish();
}

fn bench_threadlocal_rawcopy(c: &mut Criterion) {
    let mut group = c.benchmark_group("threadlocal_rawcopy");

    for num_batches in [1, 10, 100] {
        let batches: Vec<_> = (0..num_batches).map(|i| make_batch(i % 8)).collect();
        let total_events: usize = batches.iter().map(|b| b.len() * 2).sum();
        group.throughput(criterion::Throughput::Elements(total_events as u64));

        group.bench_with_input(
            BenchmarkId::new("batches", num_batches),
            &batches,
            |b, batches| {
                b.iter(|| {
                    let mut output = Vec::new();
                    dial9_trace_format::codec::encode_header(&mut output).unwrap();
                    for batch in batches {
                        let mut local = Encoder::new();
                        encode_batch(&mut local, batch);
                        let bytes = local.reset_to(Vec::new()).unwrap();
                        output.extend_from_slice(&bytes);
                    }
                    output
                });
            },
        );
    }
    group.finish();
}

criterion_group!(benches, bench_direct_encode, bench_threadlocal_rawcopy);
criterion_main!(benches);
