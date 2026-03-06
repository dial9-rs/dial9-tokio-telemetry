//! Demonstrates encoding dial9-tokio-telemetry-style events using the new
//! self-describing trace format, then decoding and printing them.
//!
//! All event types — including CpuSample with stack frames — use
//! `#[derive(TraceEvent)]` and the `enc.write(&event)` API.

use dial9_trace_format::codec::SymbolEntry;
use dial9_trace_format::decoder::{DecodedFrame, Decoder};
use dial9_trace_format::encoder::Encoder;
use dial9_trace_format::types::FieldValue;
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

    // Offline symbolization
    let sym1 = enc.intern_string("my_app::handle_request");
    let sym2 = enc.intern_string("tokio::runtime::poll");
    enc.write_symbol_table(&[
        SymbolEntry { base_addr: 0x5555_5555_0000, size: 0x0900, symbol_id: sym2.pool_id() },
        SymbolEntry { base_addr: 0x5555_5555_0900, size: 0x1000, symbol_id: sym1.pool_id() },
    ]);

    let data = enc.finish();
    println!("Encoded trace: {} bytes\n", data.len());

    // Decode and print
    let mut dec = Decoder::new(&data).unwrap();
    for frame in dec.decode_all() {
        match &frame {
            DecodedFrame::Schema(s) => {
                let fields: Vec<_> = s.fields.iter()
                    .map(|f| format!("{}: {:?}", f.name, f.field_type)).collect();
                println!("Schema[{}] \"{}\" {{ {} }}", s.type_id, s.name, fields.join(", "));
            }
            DecodedFrame::Event { type_id, values } => {
                let schema = dec.registry().get(*type_id).unwrap();
                let fields: Vec<_> = schema.fields.iter().zip(values)
                    .map(|(def, val)| format!("{}={}", def.name, format_value(val)))
                    .collect();
                println!("  {} {{ {} }}", schema.name, fields.join(", "));
            }
            DecodedFrame::StringPool(entries) => {
                for e in entries {
                    println!("Pool[{}] = {:?}", e.pool_id, String::from_utf8_lossy(&e.data));
                }
            }
            DecodedFrame::SymbolTable(entries) => {
                for e in entries {
                    let name = dec.string_pool.get(&e.symbol_id)
                        .map(|s| s.as_str()).unwrap_or("???");
                    println!("Symbol 0x{:x}..+{} = {}", e.base_addr, e.size, name);
                }
            }
        }
    }
}

fn format_value(val: &FieldValue) -> String {
    match val {
        FieldValue::U64(v) | FieldValue::Varint(v) => v.to_string(),
        FieldValue::I64(v) => v.to_string(),
        FieldValue::F64(v) => v.to_string(),
        FieldValue::Bool(v) => v.to_string(),
        FieldValue::String(v) => format!("{:?}", String::from_utf8_lossy(v)),
        FieldValue::Bytes(v) => format!("{} bytes", v.len()),
        FieldValue::U64Array(v) => format!("{v:?}"),
        FieldValue::PooledString(id) => format!("pool#{id}"),
        FieldValue::StackFrames(addrs) => {
            let hex: Vec<_> = addrs.iter().map(|a| format!("0x{a:x}")).collect();
            format!("[{}]", hex.join(", "))
        }
        FieldValue::StringMap(pairs) => {
            let kvs: Vec<_> = pairs.iter().map(|(k, v)| {
                format!("{}={}", String::from_utf8_lossy(k), String::from_utf8_lossy(v))
            }).collect();
            format!("{{{}}}", kvs.join(", "))
        }
    }
}
