//! Demonstrates encoding dial9-tokio-telemetry-style events using the new
//! self-describing trace format, then decoding them back into typed structs
//! via the macro-generated `decode()` method.

use dial9_trace_format::codec::SymbolEntry;
use dial9_trace_format::decoder::{DecodedFrameRef, Decoder};
use dial9_trace_format::encoder::Encoder;
use dial9_trace_format::{InternedString, StackFrames, TraceEvent};

#[derive(TraceEvent)]
struct PollStart {
    timestamp_ns: u64,
    worker_id: u64,
    local_queue_depth: u64,
    task_id: u64,
    spawn_loc_id: u64,
}

#[derive(TraceEvent)]
struct PollEnd {
    timestamp_ns: u64,
    worker_id: u64,
}

#[derive(TraceEvent)]
struct WorkerPark {
    timestamp_ns: u64,
    worker_id: u64,
    local_queue_depth: u64,
    cpu_time_ns: u64,
}

#[derive(TraceEvent)]
struct WorkerUnpark {
    timestamp_ns: u64,
    worker_id: u64,
    local_queue_depth: u64,
    cpu_time_ns: u64,
    sched_wait_ns: u64,
}

#[derive(TraceEvent)]
struct WakeEvent {
    timestamp_ns: u64,
    waker_task_id: u64,
    woken_task_id: u64,
    target_worker: u64,
}

#[derive(TraceEvent)]
struct CpuSample {
    timestamp_ns: u64,
    worker_id: u64,
    tid: u32,
    source: u8,
    thread_name: InternedString,
    frames: StackFrames,
}

fn main() {
    // --- Encode using derive macro types ---
    let mut enc = Encoder::new();

    enc.write(&PollStart {
        timestamp_ns: 1_000_000, worker_id: 0, local_queue_depth: 3,
        task_id: 42, spawn_loc_id: 1,
    });
    enc.write(&PollEnd { timestamp_ns: 1_050_000, worker_id: 0 });
    enc.write(&WorkerPark {
        timestamp_ns: 1_100_000, worker_id: 0, local_queue_depth: 0,
        cpu_time_ns: 500_000_000,
    });
    let thread_name = enc.intern_string("tokio-runtime-worker");
    enc.write(&CpuSample {
        timestamp_ns: 1_120_000, worker_id: 0, tid: 12345, source: 0,
        thread_name,
        frames: StackFrames(vec![0x5555_5555_1234, 0x5555_5555_0a00, 0x5555_5555_0800]),
    });
    enc.write(&WorkerUnpark {
        timestamp_ns: 1_200_000, worker_id: 0, local_queue_depth: 1,
        cpu_time_ns: 500_100_000, sched_wait_ns: 42_000,
    });
    enc.write(&WakeEvent {
        timestamp_ns: 1_200_500, waker_task_id: 42, woken_task_id: 99,
        target_worker: 1,
    });

    let sym1 = enc.intern_string("my_app::handle_request");
    let sym2 = enc.intern_string("tokio::runtime::poll");
    enc.write_symbol_table(&[
        SymbolEntry { base_addr: 0x5555_5555_0000, size: 0x0900, symbol_id: sym2.0 },
        SymbolEntry { base_addr: 0x5555_5555_0900, size: 0x1000, symbol_id: sym1.0 },
    ]);

    let data = enc.finish();
    println!("Encoded trace: {} bytes\n", data.len());

    // --- Decode back into typed structs via macro-generated decode() ---
    let mut dec = Decoder::new(&data).unwrap();
    for frame in dec.decode_all_ref() {
        match &frame {
            DecodedFrameRef::Schema(s) => {
                println!("Schema[{}] \"{}\"", s.type_id, s.name);
            }
            DecodedFrameRef::Event { type_id, values } => {
                let name = dec.registry().get(*type_id).map(|s| s.name.as_str()).unwrap_or("?");
                match name {
                    "PollStart" => println!("  {:?}", PollStart::decode(values).unwrap()),
                    "PollEnd" => println!("  {:?}", PollEnd::decode(values).unwrap()),
                    "WorkerPark" => println!("  {:?}", WorkerPark::decode(values).unwrap()),
                    "WorkerUnpark" => println!("  {:?}", WorkerUnpark::decode(values).unwrap()),
                    "WakeEvent" => println!("  {:?}", WakeEvent::decode(values).unwrap()),
                    "CpuSample" => println!("  {:?}", CpuSample::decode(values).unwrap()),
                    other => println!("  {other} (unknown)"),
                }
            }
            DecodedFrameRef::StringPool(entries) => {
                for e in entries {
                    println!("Pool[{}] = {:?}", e.pool_id, std::str::from_utf8(e.data).unwrap_or("?"));
                }
            }
            DecodedFrameRef::SymbolTable(entries) => {
                for e in entries {
                    let name = dec.string_pool.get(&e.symbol_id)
                        .map(|s| s.as_str()).unwrap_or("???");
                    println!("Symbol 0x{:x}..+{} = {}", e.base_addr, e.size, name);
                }
            }
        }
    }
}
