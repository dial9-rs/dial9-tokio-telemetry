//! IAI callgrind encode/decode micro-benchmarks over a 1M-event mix
//! (20% PollStart / PollEnd / WorkerPark / WakeEvent / CpuSample with
//! stack frames). Linux-only.
//!
//! Usage:
//!   cargo bench --bench codec_iai

use dial9_trace_format::decoder::Decoder;
use dial9_trace_format::encoder::Encoder;
use dial9_trace_format::{StackFrames, TraceEvent};
use iai_callgrind::{library_benchmark, library_benchmark_group, main};
use std::hint::black_box;

#[derive(TraceEvent)]
struct PollStart {
    #[traceevent(timestamp)]
    timestamp_ns: u64,
    worker_id: u64,
    local_queue_depth: u64,
    task_id: u64,
    spawn_loc_id: u64,
}
#[derive(TraceEvent)]
struct PollEnd {
    #[traceevent(timestamp)]
    timestamp_ns: u64,
    worker_id: u64,
}
#[derive(TraceEvent)]
struct WorkerPark {
    #[traceevent(timestamp)]
    timestamp_ns: u64,
    worker_id: u64,
    local_queue_depth: u64,
    cpu_time_ns: u64,
}
#[derive(TraceEvent)]
struct WakeEvent {
    #[traceevent(timestamp)]
    timestamp_ns: u64,
    waker_task_id: u64,
    woken_task_id: u64,
    target_worker: u64,
}
#[derive(TraceEvent)]
struct CpuSample {
    #[traceevent(timestamp)]
    timestamp_ns: u64,
    worker_id: u64,
    tid: u32,
    source: u8,
    frames: StackFrames,
}

const N: u64 = 1_000_000;

fn encode_events(enc: &mut Encoder, n: u64) {
    let mut ts: u64 = 1_000_000_000;
    for i in 0..n {
        ts += 500 + (i % 200);
        match i % 5 {
            0 => enc.write(&PollStart {
                timestamp_ns: ts,
                worker_id: i % 8,
                local_queue_depth: i % 32,
                task_id: 1000 + (i % 5000),
                spawn_loc_id: i % 20,
            }),
            1 => enc.write(&PollEnd {
                timestamp_ns: ts,
                worker_id: i % 8,
            }),
            2 => enc.write(&WorkerPark {
                timestamp_ns: ts,
                worker_id: i % 8,
                local_queue_depth: i % 16,
                cpu_time_ns: 500_000_000 + i * 100,
            }),
            3 => enc.write(&WakeEvent {
                timestamp_ns: ts,
                waker_task_id: 1000 + (i % 5000),
                woken_task_id: 1000 + ((i + 1) % 5000),
                target_worker: i % 8,
            }),
            _ => enc.write(&CpuSample {
                timestamp_ns: ts,
                worker_id: i % 8,
                tid: 12345 + (i % 4) as u32,
                source: 0,
                frames: StackFrames(vec![
                    0x5555_5555_0000 + (i % 100) * 0x10,
                    0x5555_5555_1000 + (i % 50) * 0x20,
                    0x5555_5555_2000,
                ]),
            }),
        }
        .unwrap()
    }
}

fn pre_encode(n: u64) -> Vec<u8> {
    let mut enc = Encoder::new();
    encode_events(&mut enc, n);
    enc.finish()
}

#[library_benchmark]
#[bench::events_1m(N)]
fn encode(n: u64) -> Vec<u8> {
    let mut enc = Encoder::new();
    encode_events(&mut enc, black_box(n));
    black_box(enc.finish())
}

#[library_benchmark]
#[bench::events_1m(pre_encode(N))]
fn decode_all(data: Vec<u8>) -> usize {
    let data = black_box(data);
    let mut dec = Decoder::new(&data).unwrap();
    let frames = dec.decode_all();
    black_box(frames.len())
}

#[library_benchmark]
#[bench::events_1m(pre_encode(N))]
fn decode_all_ref(data: Vec<u8>) -> usize {
    let data = black_box(data);
    let mut dec = Decoder::new(&data).unwrap();
    let frames = dec.decode_all_ref();
    black_box(frames.len())
}

#[library_benchmark]
#[bench::events_1m(pre_encode(N))]
fn for_each_event(data: Vec<u8>) -> u64 {
    let data = black_box(data);
    let mut dec = Decoder::new(&data).unwrap();
    let mut count = 0u64;
    dec.for_each_event(|_ev| {
        count += 1;
    })
    .unwrap();
    black_box(count)
}

library_benchmark_group!(
    name = codec_group;
    benchmarks = encode, decode_all, decode_all_ref, for_each_event
);
main!(library_benchmark_groups = codec_group);
