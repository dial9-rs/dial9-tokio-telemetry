//! IAI callgrind micro-benchmark for encode strategy comparison.
//!
//! Compares two encode strategies via instruction count:
//! - direct: events through a single Encoder<Vec<u8>>
//! - threadlocal_rawcopy: per-batch local Encoder, reset_to(Vec::new()),
//!   raw-copy into a central Vec<u8>
//!
//! Usage:
//!   cargo bench --bench threadlocal_encode_iai

use dial9_tokio_telemetry::telemetry::{
    PollEndEvent, PollStartEvent, TaskId, WorkerId, WorkerParkEvent, WorkerUnparkEvent,
};
use dial9_trace_format::encoder::Encoder;
use iai_callgrind::{library_benchmark, library_benchmark_group, main};
use std::hint::black_box;

type Batch = Vec<(u64, WorkerId, TaskId)>;

fn make_batch(worker: usize) -> Batch {
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

fn batches(num_batches: usize) -> Vec<Batch> {
    (0..num_batches).map(|i| make_batch(i % 8)).collect()
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

fn direct(batches: Vec<Batch>) -> Vec<u8> {
    let mut encoder = Encoder::new();
    for batch in &batches {
        encode_batch(&mut encoder, batch);
    }
    encoder.finish()
}

fn threadlocal_rawcopy(batches: Vec<Batch>) -> Vec<u8> {
    let mut output: Vec<u8> = Encoder::new().finish();
    for batch in &batches {
        let mut local = Encoder::new();
        encode_batch(&mut local, batch);
        let bytes = local.reset_to(Vec::new()).unwrap();
        output.extend_from_slice(&bytes);
    }
    output
}

#[library_benchmark]
#[bench::batches_1(batches(1))]
#[bench::batches_10(batches(10))]
#[bench::batches_100(batches(100))]
fn direct_encode(batches: Vec<Batch>) -> Vec<u8> {
    black_box(direct(black_box(batches)))
}

#[library_benchmark]
#[bench::batches_1(batches(1))]
#[bench::batches_10(batches(10))]
#[bench::batches_100(batches(100))]
fn threadlocal_rawcopy_encode(batches: Vec<Batch>) -> Vec<u8> {
    black_box(threadlocal_rawcopy(black_box(batches)))
}

library_benchmark_group!(
    name = encode_group;
    benchmarks = direct_encode, threadlocal_rawcopy_encode
);
main!(library_benchmark_groups = encode_group);
