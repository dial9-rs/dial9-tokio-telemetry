//! IAI callgrind micro-benchmark for encoder-only throughput.
//!
//! Measures the pure encode path: RawEvent → Encoder → Vec<u8>. No
//! RotatingWriter, no syscalls — instruction count reflects encoder work
//! only. Builds anywhere; only runs on Linux (valgrind dependency).
//!
//! Usage:
//!   cargo bench --bench writer_encode_iai

use dial9_tokio_telemetry::telemetry::{
    PollEndEvent, PollStartEvent, TaskId, TaskSpawnEvent, WakeEventEvent, WorkerId,
    WorkerParkEvent, WorkerUnparkEvent,
};
use dial9_trace_format::encoder::Encoder;
use iai_callgrind::{library_benchmark, library_benchmark_group, main};
use std::hint::black_box;

fn workers(num_batches: usize) -> Vec<usize> {
    (0..num_batches).map(|i| i % 8).collect()
}

fn encode(workers: Vec<usize>) -> Vec<u8> {
    let mut enc = Encoder::new();
    let loc = enc.intern_string_infallible("src/main.rs:42");

    for &worker in &workers {
        let wid = WorkerId::from(worker);
        let task = TaskId::from_u32(1);

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
    }

    enc.reset_to_infallible(Vec::new())
}

#[library_benchmark]
#[bench::batches_1(workers(1))]
#[bench::batches_10(workers(10))]
#[bench::batches_100(workers(100))]
fn writer_encode(workers: Vec<usize>) -> Vec<u8> {
    black_box(encode(black_box(workers)))
}

library_benchmark_group!(name = encode_group; benchmarks = writer_encode);
main!(library_benchmark_groups = encode_group);
