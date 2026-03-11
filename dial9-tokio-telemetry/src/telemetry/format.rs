//! Binary trace format backed by `dial9-trace-format`.
//!
//! This module replaces the hand-coded TOKIOTRC wire format with the
//! self-describing `dial9-trace-format` encoder/decoder. The `TelemetryEvent`
//! enum remains the in-memory representation; this module handles conversion
//! to/from the wire format via `#[derive(TraceEvent)]` structs.

use crate::telemetry::events::{CpuSampleSource, TelemetryEvent};
use crate::telemetry::task_metadata::{SpawnLocationId, TaskId};
use dial9_trace_format::encoder::Encoder;
use dial9_trace_format::types::{
    EventEncoder, FieldType, FieldValueRef,
};
use dial9_trace_format::{StackFrames, TraceEvent, TraceField};
use std::io::{self, Write};

// ── TraceField impls for newtypes ───────────────────────────────────────────

impl TraceField for TaskId {
    type Ref<'a> = TaskId;
    fn field_type() -> FieldType { FieldType::U32 }
    fn encode<W: Write>(&self, enc: &mut EventEncoder<'_, W>) -> io::Result<()> {
        enc.write_u32(self.to_u32())
    }
    fn decode_ref<'a>(val: &FieldValueRef<'a>) -> Option<Self::Ref<'a>> {
        match val {
            FieldValueRef::Varint(v) => Some(TaskId::from_u32(*v as u32)),
            _ => None,
        }
    }
}

impl TraceField for SpawnLocationId {
    type Ref<'a> = SpawnLocationId;
    fn field_type() -> FieldType { FieldType::U16 }
    fn encode<W: Write>(&self, enc: &mut EventEncoder<'_, W>) -> io::Result<()> {
        enc.write_u16(self.as_u16())
    }
    fn decode_ref<'a>(val: &FieldValueRef<'a>) -> Option<Self::Ref<'a>> {
        match val {
            FieldValueRef::Varint(v) => Some(SpawnLocationId::from_u16(*v as u16)),
            _ => None,
        }
    }
}

impl TraceField for CpuSampleSource {
    type Ref<'a> = CpuSampleSource;
    fn field_type() -> FieldType { FieldType::U8 }
    fn encode<W: Write>(&self, enc: &mut EventEncoder<'_, W>) -> io::Result<()> {
        enc.write_u8(*self as u8)
    }
    fn decode_ref<'a>(val: &FieldValueRef<'a>) -> Option<Self::Ref<'a>> {
        match val {
            FieldValueRef::Varint(v) => Some(CpuSampleSource::from_u8(*v as u8)),
            _ => None,
        }
    }
}

// ── Derive structs ──────────────────────────────────────────────────────────

#[derive(TraceEvent)]
struct PollStartEvent {
    #[traceevent(timestamp)]
    timestamp_ns: u64,
    worker_id: u8,
    local_queue: u8,
    task_id: TaskId,
    spawn_loc_id: SpawnLocationId,
}

#[derive(TraceEvent)]
struct PollEndEvent {
    #[traceevent(timestamp)]
    timestamp_ns: u64,
    worker_id: u8,
}

#[derive(TraceEvent)]
struct WorkerParkEvent {
    #[traceevent(timestamp)]
    timestamp_ns: u64,
    worker_id: u8,
    local_queue: u8,
    cpu_time_ns: u64,
}

#[derive(TraceEvent)]
struct WorkerUnparkEvent {
    #[traceevent(timestamp)]
    timestamp_ns: u64,
    worker_id: u8,
    local_queue: u8,
    cpu_time_ns: u64,
    sched_wait_ns: u64,
}

#[derive(TraceEvent)]
struct QueueSampleEvent {
    #[traceevent(timestamp)]
    timestamp_ns: u64,
    global_queue: u8,
}

#[derive(TraceEvent)]
struct SpawnLocationDefEvent {
    id: SpawnLocationId,
    location: String,
}

#[derive(TraceEvent)]
struct TaskSpawnEvent {
    #[traceevent(timestamp)]
    timestamp_ns: u64,
    task_id: TaskId,
    spawn_loc_id: SpawnLocationId,
}

#[derive(TraceEvent)]
struct TaskTerminateEvent {
    #[traceevent(timestamp)]
    timestamp_ns: u64,
    task_id: TaskId,
}

#[derive(TraceEvent)]
struct CpuSampleEvent {
    #[traceevent(timestamp)]
    timestamp_ns: u64,
    worker_id: u8,
    tid: u32,
    source: CpuSampleSource,
    callchain: StackFrames,
}

#[derive(TraceEvent)]
struct CallframeDefEvent {
    address: u64,
    symbol: String,
    /// Empty string encodes None.
    location: String,
}

#[derive(TraceEvent)]
struct ThreadNameDefEvent {
    tid: u32,
    name: String,
}

#[derive(TraceEvent)]
struct WakeEventEvent {
    #[traceevent(timestamp)]
    timestamp_ns: u64,
    waker_task_id: TaskId,
    woken_task_id: TaskId,
    target_worker: u8,
}

#[derive(TraceEvent)]
struct SegmentMetadataEvent {
    entries: Vec<(String, String)>,
}

// ── Encode ──────────────────────────────────────────────────────────────────

/// Register all event schemas on an encoder. Must be called once per encoder
/// (i.e. once per file). Uses the derive-generated schemas.
pub fn register_schemas<W: Write>(enc: &mut Encoder<W>) -> io::Result<()> {
    // Writing a dummy event of each type triggers auto-registration of its
    // schema. But Encoder::write auto-registers on first use, so we can just
    // let write_event handle it lazily. However, for the preamble to be
    // deterministic (needed for CountingWriter byte tracking), we force
    // registration up front by writing zero-sized events to a throwaway
    // encoder... Actually, the simplest approach: just call
    // lookup_or_register for each type. That's what `write` does internally.
    // Since we can't call that directly, we rely on the fact that the first
    // write_event call for each type will register the schema. The preamble
    // size will vary slightly but CountingWriter tracks actual bytes.
    //
    // For deterministic preamble, we register explicitly:
    fn register<T: TraceEvent + 'static, W: Write>(enc: &mut Encoder<W>) -> io::Result<()> {
        let entry = T::schema_entry();
        enc.register_schema_for_with_timestamp::<T>(
            T::event_name(),
            T::has_timestamp(),
            entry.fields,
        )?;
        Ok(())
    }
    register::<PollStartEvent, _>(enc)?;
    register::<PollEndEvent, _>(enc)?;
    register::<WorkerParkEvent, _>(enc)?;
    register::<WorkerUnparkEvent, _>(enc)?;
    register::<QueueSampleEvent, _>(enc)?;
    register::<SpawnLocationDefEvent, _>(enc)?;
    register::<TaskSpawnEvent, _>(enc)?;
    register::<TaskTerminateEvent, _>(enc)?;
    register::<CpuSampleEvent, _>(enc)?;
    register::<CallframeDefEvent, _>(enc)?;
    register::<ThreadNameDefEvent, _>(enc)?;
    register::<WakeEventEvent, _>(enc)?;
    register::<SegmentMetadataEvent, _>(enc)?;
    Ok(())
}

/// Write a single `TelemetryEvent` to the encoder.
pub fn write_event<W: Write>(enc: &mut Encoder<W>, event: &TelemetryEvent) -> io::Result<()> {
    match event {
        TelemetryEvent::PollStart {
            timestamp_nanos, worker_id, worker_local_queue_depth, task_id, spawn_loc_id,
        } => enc.write(&PollStartEvent {
            timestamp_ns: *timestamp_nanos,
            worker_id: *worker_id as u8,
            local_queue: *worker_local_queue_depth as u8,
            task_id: *task_id,
            spawn_loc_id: *spawn_loc_id,
        }),
        TelemetryEvent::PollEnd { timestamp_nanos, worker_id } => enc.write(&PollEndEvent {
            timestamp_ns: *timestamp_nanos,
            worker_id: *worker_id as u8,
        }),
        TelemetryEvent::WorkerPark {
            timestamp_nanos, worker_id, worker_local_queue_depth, cpu_time_nanos,
        } => enc.write(&WorkerParkEvent {
            timestamp_ns: *timestamp_nanos,
            worker_id: *worker_id as u8,
            local_queue: *worker_local_queue_depth as u8,
            cpu_time_ns: *cpu_time_nanos,
        }),
        TelemetryEvent::WorkerUnpark {
            timestamp_nanos, worker_id, worker_local_queue_depth, cpu_time_nanos, sched_wait_delta_nanos,
        } => enc.write(&WorkerUnparkEvent {
            timestamp_ns: *timestamp_nanos,
            worker_id: *worker_id as u8,
            local_queue: *worker_local_queue_depth as u8,
            cpu_time_ns: *cpu_time_nanos,
            sched_wait_ns: *sched_wait_delta_nanos,
        }),
        TelemetryEvent::QueueSample { timestamp_nanos, global_queue_depth } => {
            enc.write(&QueueSampleEvent {
                timestamp_ns: *timestamp_nanos,
                global_queue: *global_queue_depth as u8,
            })
        }
        TelemetryEvent::SpawnLocationDef { id, location } => {
            enc.write(&SpawnLocationDefEvent { id: *id, location: location.clone() })
        }
        TelemetryEvent::TaskSpawn { timestamp_nanos, task_id, spawn_loc_id } => {
            enc.write(&TaskSpawnEvent {
                timestamp_ns: *timestamp_nanos,
                task_id: *task_id,
                spawn_loc_id: *spawn_loc_id,
            })
        }
        TelemetryEvent::TaskTerminate { timestamp_nanos, task_id } => {
            enc.write(&TaskTerminateEvent { timestamp_ns: *timestamp_nanos, task_id: *task_id })
        }
        TelemetryEvent::CpuSample {
            timestamp_nanos, worker_id, tid, source, callchain,
        } => enc.write(&CpuSampleEvent {
            timestamp_ns: *timestamp_nanos,
            worker_id: *worker_id as u8,
            tid: *tid,
            source: *source,
            callchain: StackFrames(callchain.clone()),
        }),
        TelemetryEvent::CallframeDef { address, symbol, location } => {
            enc.write(&CallframeDefEvent {
                address: *address,
                symbol: symbol.clone(),
                location: location.as_deref().unwrap_or("").to_string(),
            })
        }
        TelemetryEvent::ThreadNameDef { tid, name } => {
            enc.write(&ThreadNameDefEvent { tid: *tid, name: name.clone() })
        }
        TelemetryEvent::WakeEvent {
            timestamp_nanos, waker_task_id, woken_task_id, target_worker,
        } => enc.write(&WakeEventEvent {
            timestamp_ns: *timestamp_nanos,
            waker_task_id: *waker_task_id,
            woken_task_id: *woken_task_id,
            target_worker: *target_worker,
        }),
        TelemetryEvent::SegmentMetadata { entries } => {
            enc.write(&SegmentMetadataEvent { entries: entries.clone() })
        }
    }
}

// ── Decode ──────────────────────────────────────────────────────────────────

/// Decode all events from a byte slice into `TelemetryEvent`s.
pub fn decode_events(data: &[u8]) -> io::Result<Vec<TelemetryEvent>> {
    use dial9_trace_format::decoder::{DecodedFrame, Decoder};

    let mut dec =
        Decoder::new(data).ok_or_else(|| io::Error::new(io::ErrorKind::InvalidData, "invalid trace header"))?;
    let mut events = Vec::new();

    for frame in dec.decode_all() {
        if let DecodedFrame::Event { type_id, timestamp_ns, values } = frame {
            let name = match dec.registry().get(type_id) {
                Some(s) => &s.name,
                None => continue,
            };
            if let Some(ev) = decode_one(name, timestamp_ns, &values) {
                events.push(ev);
            }
        }
    }
    Ok(events)
}

fn v(values: &[dial9_trace_format::types::FieldValue], i: usize) -> u64 {
    match values.get(i) {
        Some(dial9_trace_format::types::FieldValue::Varint(n)) => *n,
        _ => 0,
    }
}

fn s(values: &[dial9_trace_format::types::FieldValue], i: usize) -> String {
    match values.get(i) {
        Some(dial9_trace_format::types::FieldValue::String(s)) => s.clone(),
        _ => String::new(),
    }
}

fn decode_one(
    name: &str,
    timestamp_ns: Option<u64>,
    values: &[dial9_trace_format::types::FieldValue],
) -> Option<TelemetryEvent> {
    let ts = timestamp_ns.unwrap_or(0);
    Some(match name {
        "PollStartEvent" => TelemetryEvent::PollStart {
            timestamp_nanos: ts,
            worker_id: v(values, 0) as usize,
            worker_local_queue_depth: v(values, 1) as usize,
            task_id: TaskId::from_u32(v(values, 2) as u32),
            spawn_loc_id: SpawnLocationId::from_u16(v(values, 3) as u16),
        },
        "PollEndEvent" => TelemetryEvent::PollEnd {
            timestamp_nanos: ts,
            worker_id: v(values, 0) as usize,
        },
        "WorkerParkEvent" => TelemetryEvent::WorkerPark {
            timestamp_nanos: ts,
            worker_id: v(values, 0) as usize,
            worker_local_queue_depth: v(values, 1) as usize,
            cpu_time_nanos: v(values, 2),
        },
        "WorkerUnparkEvent" => TelemetryEvent::WorkerUnpark {
            timestamp_nanos: ts,
            worker_id: v(values, 0) as usize,
            worker_local_queue_depth: v(values, 1) as usize,
            cpu_time_nanos: v(values, 2),
            sched_wait_delta_nanos: v(values, 3),
        },
        "QueueSampleEvent" => TelemetryEvent::QueueSample {
            timestamp_nanos: ts,
            global_queue_depth: v(values, 0) as usize,
        },
        "SpawnLocationDefEvent" => TelemetryEvent::SpawnLocationDef {
            id: SpawnLocationId::from_u16(v(values, 0) as u16),
            location: s(values, 1),
        },
        "TaskSpawnEvent" => TelemetryEvent::TaskSpawn {
            timestamp_nanos: ts,
            task_id: TaskId::from_u32(v(values, 0) as u32),
            spawn_loc_id: SpawnLocationId::from_u16(v(values, 1) as u16),
        },
        "TaskTerminateEvent" => TelemetryEvent::TaskTerminate {
            timestamp_nanos: ts,
            task_id: TaskId::from_u32(v(values, 0) as u32),
        },
        "CpuSampleEvent" => {
            let callchain = match values.get(3) {
                Some(dial9_trace_format::types::FieldValue::StackFrames(f)) => f.clone(),
                _ => vec![],
            };
            TelemetryEvent::CpuSample {
                timestamp_nanos: ts,
                worker_id: v(values, 0) as usize,
                tid: v(values, 1) as u32,
                source: CpuSampleSource::from_u8(v(values, 2) as u8),
                callchain,
            }
        }
        "CallframeDefEvent" => {
            let loc = s(values, 2);
            TelemetryEvent::CallframeDef {
                address: v(values, 0),
                symbol: s(values, 1),
                location: if loc.is_empty() { None } else { Some(loc) },
            }
        }
        "ThreadNameDefEvent" => TelemetryEvent::ThreadNameDef {
            tid: v(values, 0) as u32,
            name: s(values, 1),
        },
        "WakeEventEvent" => TelemetryEvent::WakeEvent {
            timestamp_nanos: ts,
            waker_task_id: TaskId::from_u32(v(values, 0) as u32),
            woken_task_id: TaskId::from_u32(v(values, 1) as u32),
            target_worker: v(values, 2) as u8,
        },
        "SegmentMetadataEvent" => {
            let entries = match values.first() {
                Some(dial9_trace_format::types::FieldValue::StringMap(map)) => map
                    .iter()
                    .filter_map(|(k, val)| {
                        Some((String::from_utf8(k.clone()).ok()?, String::from_utf8(val.clone()).ok()?))
                    })
                    .collect(),
                _ => vec![],
            };
            TelemetryEvent::SegmentMetadata { entries }
        }
        _ => return None,
    })
}

// ── Tests ───────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;
    use crate::telemetry::task_metadata::{SpawnLocationId, TaskId};

    fn roundtrip(event: &TelemetryEvent) -> TelemetryEvent {
        let mut enc = Encoder::new();
        register_schemas(&mut enc).unwrap();
        write_event(&mut enc, event).unwrap();
        let bytes = enc.finish();
        let events = decode_events(&bytes).unwrap();
        assert_eq!(events.len(), 1, "expected 1 event, got {}", events.len());
        events.into_iter().next().unwrap()
    }

    #[test]
    fn poll_start_roundtrip() {
        let event = TelemetryEvent::PollStart {
            timestamp_nanos: 123_456_000,
            worker_id: 3,
            worker_local_queue_depth: 17,
            task_id: TaskId::from_u32(42),
            spawn_loc_id: SpawnLocationId::from_u16(5),
        };
        assert_eq!(roundtrip(&event), event);
    }

    #[test]
    fn poll_end_roundtrip() {
        let event = TelemetryEvent::PollEnd { timestamp_nanos: 999_000, worker_id: 1 };
        assert_eq!(roundtrip(&event), event);
    }

    #[test]
    fn worker_park_roundtrip() {
        let event = TelemetryEvent::WorkerPark {
            timestamp_nanos: 5_000_000_000,
            worker_id: 7,
            worker_local_queue_depth: 200,
            cpu_time_nanos: 1_234_567_000,
        };
        assert_eq!(roundtrip(&event), event);
    }

    #[test]
    fn worker_unpark_roundtrip() {
        let event = TelemetryEvent::WorkerUnpark {
            timestamp_nanos: 1_000_000,
            worker_id: 2,
            worker_local_queue_depth: 55,
            cpu_time_nanos: 999_000,
            sched_wait_delta_nanos: 42_000,
        };
        assert_eq!(roundtrip(&event), event);
    }

    #[test]
    fn queue_sample_roundtrip() {
        let event = TelemetryEvent::QueueSample {
            timestamp_nanos: 10_000_000_000,
            global_queue_depth: 128,
        };
        assert_eq!(roundtrip(&event), event);
    }

    #[test]
    fn spawn_location_def_roundtrip() {
        let event = TelemetryEvent::SpawnLocationDef {
            id: SpawnLocationId::from_u16(42),
            location: "src/main.rs:123:45".to_string(),
        };
        assert_eq!(roundtrip(&event), event);
    }

    #[test]
    fn task_spawn_roundtrip() {
        let event = TelemetryEvent::TaskSpawn {
            timestamp_nanos: 5_000_000,
            task_id: TaskId::from_u32(99),
            spawn_loc_id: SpawnLocationId::from_u16(7),
        };
        assert_eq!(roundtrip(&event), event);
    }

    #[test]
    fn task_terminate_roundtrip() {
        let event = TelemetryEvent::TaskTerminate {
            timestamp_nanos: 5_000_000,
            task_id: TaskId::from_u32(42),
        };
        assert_eq!(roundtrip(&event), event);
    }

    #[test]
    fn cpu_sample_roundtrip() {
        let event = TelemetryEvent::CpuSample {
            timestamp_nanos: 1_000_000,
            worker_id: 0,
            tid: 12345,
            source: CpuSampleSource::CpuProfile,
            callchain: vec![0x5555_1234, 0x5555_0a00],
        };
        assert_eq!(roundtrip(&event), event);
    }

    #[test]
    fn callframe_def_roundtrip() {
        let event = TelemetryEvent::CallframeDef {
            address: 0x5555_1234,
            symbol: "my_crate::my_function".to_string(),
            location: Some("src/lib.rs:42".to_string()),
        };
        assert_eq!(roundtrip(&event), event);
    }

    #[test]
    fn callframe_def_no_location_roundtrip() {
        let event = TelemetryEvent::CallframeDef {
            address: 0x5555_1234,
            symbol: "my_crate::my_function".to_string(),
            location: None,
        };
        assert_eq!(roundtrip(&event), event);
    }

    #[test]
    fn thread_name_def_roundtrip() {
        let event = TelemetryEvent::ThreadNameDef {
            tid: 12345,
            name: "tokio-runtime-worker".to_string(),
        };
        assert_eq!(roundtrip(&event), event);
    }

    #[test]
    fn wake_event_roundtrip() {
        let event = TelemetryEvent::WakeEvent {
            timestamp_nanos: 5_000_000,
            waker_task_id: TaskId::from_u32(10),
            woken_task_id: TaskId::from_u32(20),
            target_worker: 3,
        };
        assert_eq!(roundtrip(&event), event);
    }

    #[test]
    fn segment_metadata_roundtrip() {
        let event = TelemetryEvent::SegmentMetadata {
            entries: vec![
                ("service".into(), "checkout-api".into()),
                ("host".into(), "i-0abc123".into()),
            ],
        };
        assert_eq!(roundtrip(&event), event);
    }

    #[test]
    fn segment_metadata_empty_roundtrip() {
        let event = TelemetryEvent::SegmentMetadata { entries: vec![] };
        assert_eq!(roundtrip(&event), event);
    }

    #[test]
    fn mixed_event_stream() {
        let events = vec![
            TelemetryEvent::SpawnLocationDef {
                id: SpawnLocationId::from_u16(1),
                location: "src/main.rs:10:1".to_string(),
            },
            TelemetryEvent::TaskSpawn {
                timestamp_nanos: 500_000,
                task_id: TaskId::from_u32(100),
                spawn_loc_id: SpawnLocationId::from_u16(1),
            },
            TelemetryEvent::PollStart {
                timestamp_nanos: 1_000_000,
                worker_id: 0,
                worker_local_queue_depth: 3,
                task_id: TaskId::from_u32(100),
                spawn_loc_id: SpawnLocationId::from_u16(1),
            },
            TelemetryEvent::PollEnd { timestamp_nanos: 2_000_000, worker_id: 0 },
        ];

        let mut enc = Encoder::new();
        register_schemas(&mut enc).unwrap();
        for e in &events {
            write_event(&mut enc, e).unwrap();
        }
        let bytes = enc.finish();
        let decoded = decode_events(&bytes).unwrap();
        assert_eq!(decoded, events);
    }
}
