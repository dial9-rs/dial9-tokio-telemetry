//! Convert a TOKIOTRC binary trace to "fat" JSONL with all metadata resolved inline.
//!
//! Usage:
//!   cargo run --example trace_to_fat_jsonl -- <input.bin> [output.jsonl]

use dial9_tokio_telemetry::telemetry::{TraceReader, events::TelemetryEvent};
use serde::Serialize;
use std::io::{BufWriter, Write};

#[derive(Serialize)]
#[serde(tag = "event")]
enum FatEvent {
    PollStart {
        timestamp_ns: u64,
        worker: usize,
        local_q: usize,
        task_id: u32,
        spawn_location: Option<String>,
    },
    PollEnd {
        timestamp_ns: u64,
        worker: usize,
    },
    WorkerPark {
        timestamp_ns: u64,
        worker: usize,
        local_q: usize,
        cpu_ns: u64,
    },
    WorkerUnpark {
        timestamp_ns: u64,
        worker: usize,
        local_q: usize,
        cpu_ns: u64,
        sched_wait_ns: u64,
    },
    QueueSample {
        timestamp_ns: u64,
        global_q: usize,
    },
    CpuSample {
        timestamp_ns: u64,
        worker: usize,
        source: String,
        callchain: Vec<String>,
    },
    WakeEvent {
        timestamp_ns: u64,
        waker_task_id: u32,
        woken_task_id: u32,
        target_worker: u8,
    },
}

fn main() -> std::io::Result<()> {
    let args: Vec<String> = std::env::args().collect();
    if args.len() < 2 {
        eprintln!("usage: trace_to_fat_jsonl <input.bin> [output.jsonl]");
        std::process::exit(1);
    }

    let mut reader = TraceReader::new(&args[1])?;
    let (magic, version) = reader.read_header()?;
    if magic != "TOKIOTRC" {
        eprintln!("not a TOKIOTRC file (got: {magic})");
        std::process::exit(1);
    }
    eprintln!("TOKIOTRC v{version}, converting to fat events...");

    let out: Box<dyn Write> = if let Some(path) = args.get(2) {
        Box::new(std::fs::File::create(path)?)
    } else {
        Box::new(std::io::stdout().lock())
    };
    let mut w = BufWriter::new(out);

    let mut count = 0u64;
    while let Some(e) = reader.read_raw_event()? {
        if let Some(fat) = to_fat_event(&e, &reader) {
            serde_json::to_writer(&mut w, &fat)
                .map_err(|e| std::io::Error::new(std::io::ErrorKind::Other, e))?;
            w.write_all(b"\n")?;
            count += 1;
        }
    }
    w.flush()?;
    eprintln!("{count} events written");
    Ok(())
}

fn to_fat_event(event: &TelemetryEvent, reader: &TraceReader) -> Option<FatEvent> {
    match event {
        TelemetryEvent::PollStart {
            timestamp_nanos,
            worker_id,
            worker_local_queue_depth,
            task_id,
            spawn_loc_id,
        } => Some(FatEvent::PollStart {
            timestamp_ns: *timestamp_nanos,
            worker: *worker_id,
            local_q: *worker_local_queue_depth,
            task_id: task_id.to_u32(),
            spawn_location: reader.spawn_locations.get(spawn_loc_id).cloned(),
        }),
        TelemetryEvent::PollEnd {
            timestamp_nanos,
            worker_id,
        } => Some(FatEvent::PollEnd {
            timestamp_ns: *timestamp_nanos,
            worker: *worker_id,
        }),
        TelemetryEvent::WorkerPark {
            timestamp_nanos,
            worker_id,
            worker_local_queue_depth,
            cpu_time_nanos,
        } => Some(FatEvent::WorkerPark {
            timestamp_ns: *timestamp_nanos,
            worker: *worker_id,
            local_q: *worker_local_queue_depth,
            cpu_ns: *cpu_time_nanos,
        }),
        TelemetryEvent::WorkerUnpark {
            timestamp_nanos,
            worker_id,
            worker_local_queue_depth,
            cpu_time_nanos,
            sched_wait_delta_nanos,
        } => Some(FatEvent::WorkerUnpark {
            timestamp_ns: *timestamp_nanos,
            worker: *worker_id,
            local_q: *worker_local_queue_depth,
            cpu_ns: *cpu_time_nanos,
            sched_wait_ns: *sched_wait_delta_nanos,
        }),
        TelemetryEvent::QueueSample {
            timestamp_nanos,
            global_queue_depth,
        } => Some(FatEvent::QueueSample {
            timestamp_ns: *timestamp_nanos,
            global_q: *global_queue_depth,
        }),
        TelemetryEvent::CpuSample {
            timestamp_nanos,
            worker_id,
            source,
            callchain,
            ..
        } => Some(FatEvent::CpuSample {
            timestamp_ns: *timestamp_nanos,
            worker: *worker_id,
            source: format!("{:?}", source),
            callchain: callchain
                .iter()
                .map(|addr| {
                    reader
                        .callframe_symbols
                        .get(addr)
                        .cloned()
                        .unwrap_or_else(|| format!("0x{:x}", addr))
                })
                .collect(),
        }),
        TelemetryEvent::WakeEvent {
            timestamp_nanos,
            waker_task_id,
            woken_task_id,
            target_worker,
        } => Some(FatEvent::WakeEvent {
            timestamp_ns: *timestamp_nanos,
            waker_task_id: waker_task_id.to_u32(),
            woken_task_id: woken_task_id.to_u32(),
            target_worker: *target_worker,
        }),
        TelemetryEvent::SpawnLocationDef { .. }
        | TelemetryEvent::TaskSpawn { .. }
        | TelemetryEvent::CallframeDef { .. }
        | TelemetryEvent::ThreadNameDef { .. } => None,
    }
}
