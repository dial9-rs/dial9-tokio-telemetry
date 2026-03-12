//! Binary trace format backed by `dial9-trace-format`.
//!
//! This module replaces the hand-coded TOKIOTRC wire format with the
//! self-describing `dial9-trace-format` encoder/decoder. The `TelemetryEvent`
//! enum remains the in-memory representation; this module handles conversion
//! to/from the wire format via `#[derive(TraceEvent)]` structs.

use crate::telemetry::events::{CpuSampleSource, TelemetryEvent};
use crate::telemetry::task_metadata::TaskId;
use dial9_trace_format::encoder::Encoder;
use dial9_trace_format::types::{
    EventEncoder, FieldType, FieldValueRef,
};
use dial9_trace_format::{InternedString, StackFrames, TraceEvent, TraceField};
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
pub struct PollStartEvent {
    #[traceevent(timestamp)]
    pub timestamp_ns: u64,
    pub worker_id: u8,
    pub local_queue: u8,
    pub task_id: TaskId,
    pub spawn_loc_id: InternedString,
}

#[derive(TraceEvent)]
pub struct PollEndEvent {
    #[traceevent(timestamp)]
    pub timestamp_ns: u64,
    pub worker_id: u8,
}

#[derive(TraceEvent)]
pub struct WorkerParkEvent {
    #[traceevent(timestamp)]
    pub timestamp_ns: u64,
    pub worker_id: u8,
    pub local_queue: u8,
    pub cpu_time_ns: u64,
}

#[derive(TraceEvent)]
pub struct WorkerUnparkEvent {
    #[traceevent(timestamp)]
    pub timestamp_ns: u64,
    pub worker_id: u8,
    pub local_queue: u8,
    pub cpu_time_ns: u64,
    pub sched_wait_ns: u64,
}

#[derive(TraceEvent)]
pub struct QueueSampleEvent {
    #[traceevent(timestamp)]
    pub timestamp_ns: u64,
    pub global_queue: u8,
}

#[derive(TraceEvent)]
pub struct TaskSpawnEvent {
    #[traceevent(timestamp)]
    pub timestamp_ns: u64,
    pub task_id: TaskId,
    pub spawn_loc_id: InternedString,
}

#[derive(TraceEvent)]
pub struct TaskTerminateEvent {
    #[traceevent(timestamp)]
    pub timestamp_ns: u64,
    pub task_id: TaskId,
}

#[derive(TraceEvent)]
pub struct CpuSampleEvent {
    #[traceevent(timestamp)]
    pub timestamp_ns: u64,
    pub worker_id: u8,
    pub tid: u32,
    pub source: CpuSampleSource,
    pub callchain: StackFrames,
}

#[derive(TraceEvent)]
pub struct CallframeDefEvent {
    pub address: u64,
    pub symbol: String,
    /// Empty string encodes None.
    pub location: String,
}

#[derive(TraceEvent)]
pub struct ThreadNameDefEvent {
    pub tid: u32,
    pub name: String,
}

#[derive(TraceEvent)]
pub struct WakeEventEvent {
    #[traceevent(timestamp)]
    pub timestamp_ns: u64,
    pub waker_task_id: TaskId,
    pub woken_task_id: TaskId,
    pub target_worker: u8,
}

#[derive(TraceEvent)]
pub struct SegmentMetadataEvent {
    pub entries: Vec<(String, String)>,
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
    Ok(decode_events_ref(data)?.into_iter().map(TelemetryEvent::from).collect())
}

/// Decode all events from a byte slice into zero-copy [`TelemetryEventRef`]s.
/// The returned refs borrow from `data`.
pub fn decode_events_ref(data: &[u8]) -> io::Result<Vec<TelemetryEventRef<'_>>> {
    use dial9_trace_format::decoder::Decoder;

    let mut dec =
        Decoder::new(data).ok_or_else(|| io::Error::new(io::ErrorKind::InvalidData, "invalid trace header"))?;
    let mut events = Vec::new();

    dec.for_each_event(|ev| {
        if let Some(ev) = decode_ref(ev.name, ev.timestamp_ns, ev.fields) {
            events.push(ev);
        }
    }).map_err(|e| io::Error::new(io::ErrorKind::InvalidData, e.to_string()))?;

    Ok(events)
}

// ── Zero-copy typed decode ──────────────────────────────────────────────────

/// Zero-copy enum of all telemetry event types. Each variant wraps the
/// derive-generated `*EventRef<'a>` that borrows directly from the decode
/// buffer. Timestamped events carry their timestamp inside the Ref struct.
#[derive(Debug, Clone)]
pub enum TelemetryEventRef<'a> {
    PollStart(PollStartEventRef<'a>),
    PollEnd(PollEndEventRef<'a>),
    WorkerPark(WorkerParkEventRef<'a>),
    WorkerUnpark(WorkerUnparkEventRef<'a>),
    QueueSample(QueueSampleEventRef<'a>),
    TaskSpawn(TaskSpawnEventRef<'a>),
    TaskTerminate(TaskTerminateEventRef<'a>),
    CpuSample(CpuSampleEventRef<'a>),
    CallframeDef(CallframeDefEventRef<'a>),
    ThreadNameDef(ThreadNameDefEventRef<'a>),
    WakeEvent(WakeEventEventRef<'a>),
    SegmentMetadata(SegmentMetadataEventRef<'a>),
}

impl<'a> TelemetryEventRef<'a> {
    /// Returns the timestamp in nanoseconds, if this event type carries one.
    pub fn timestamp_ns(&self) -> Option<u64> {
        match self {
            Self::PollStart(e) => Some(e.timestamp_ns),
            Self::PollEnd(e) => Some(e.timestamp_ns),
            Self::WorkerPark(e) => Some(e.timestamp_ns),
            Self::WorkerUnpark(e) => Some(e.timestamp_ns),
            Self::QueueSample(e) => Some(e.timestamp_ns),
            Self::TaskSpawn(e) => Some(e.timestamp_ns),
            Self::TaskTerminate(e) => Some(e.timestamp_ns),
            Self::CpuSample(e) => Some(e.timestamp_ns),
            Self::WakeEvent(e) => Some(e.timestamp_ns),
            Self::CallframeDef(_)
            | Self::ThreadNameDef(_)
            | Self::SegmentMetadata(_) => None,
        }
    }
}

/// Decode a single event from its schema name and zero-copy field values into
/// a [`TelemetryEventRef`]. Returns `None` for unknown event names.
pub fn decode_ref<'a>(
    name: &str,
    timestamp_ns: Option<u64>,
    fields: &[dial9_trace_format::types::FieldValueRef<'a>],
) -> Option<TelemetryEventRef<'a>> {
    use dial9_trace_format::TraceEvent as _;
    Some(match name {
        "PollStartEvent" => TelemetryEventRef::PollStart(PollStartEvent::decode(timestamp_ns, fields)?),
        "PollEndEvent" => TelemetryEventRef::PollEnd(PollEndEvent::decode(timestamp_ns, fields)?),
        "WorkerParkEvent" => TelemetryEventRef::WorkerPark(WorkerParkEvent::decode(timestamp_ns, fields)?),
        "WorkerUnparkEvent" => TelemetryEventRef::WorkerUnpark(WorkerUnparkEvent::decode(timestamp_ns, fields)?),
        "QueueSampleEvent" => TelemetryEventRef::QueueSample(QueueSampleEvent::decode(timestamp_ns, fields)?),
        "TaskSpawnEvent" => TelemetryEventRef::TaskSpawn(TaskSpawnEvent::decode(timestamp_ns, fields)?),
        "TaskTerminateEvent" => TelemetryEventRef::TaskTerminate(TaskTerminateEvent::decode(timestamp_ns, fields)?),
        "CpuSampleEvent" => TelemetryEventRef::CpuSample(CpuSampleEvent::decode(timestamp_ns, fields)?),
        "CallframeDefEvent" => TelemetryEventRef::CallframeDef(CallframeDefEvent::decode(timestamp_ns, fields)?),
        "ThreadNameDefEvent" => TelemetryEventRef::ThreadNameDef(ThreadNameDefEvent::decode(timestamp_ns, fields)?),
        "WakeEventEvent" => TelemetryEventRef::WakeEvent(WakeEventEvent::decode(timestamp_ns, fields)?),
        "SegmentMetadataEvent" => TelemetryEventRef::SegmentMetadata(SegmentMetadataEvent::decode(timestamp_ns, fields)?),
        _ => return None,
    })
}

impl From<TelemetryEventRef<'_>> for TelemetryEvent {
    fn from(r: TelemetryEventRef<'_>) -> Self {
        match r {
            TelemetryEventRef::PollStart(e) => TelemetryEvent::PollStart {
                timestamp_nanos: e.timestamp_ns,
                worker_id: e.worker_id as usize,
                worker_local_queue_depth: e.local_queue as usize,
                task_id: e.task_id,
                spawn_loc_id: e.spawn_loc_id,
            },
            TelemetryEventRef::PollEnd(e) => TelemetryEvent::PollEnd {
                timestamp_nanos: e.timestamp_ns,
                worker_id: e.worker_id as usize,
            },
            TelemetryEventRef::WorkerPark(e) => TelemetryEvent::WorkerPark {
                timestamp_nanos: e.timestamp_ns,
                worker_id: e.worker_id as usize,
                worker_local_queue_depth: e.local_queue as usize,
                cpu_time_nanos: e.cpu_time_ns,
            },
            TelemetryEventRef::WorkerUnpark(e) => TelemetryEvent::WorkerUnpark {
                timestamp_nanos: e.timestamp_ns,
                worker_id: e.worker_id as usize,
                worker_local_queue_depth: e.local_queue as usize,
                cpu_time_nanos: e.cpu_time_ns,
                sched_wait_delta_nanos: e.sched_wait_ns,
            },
            TelemetryEventRef::QueueSample(e) => TelemetryEvent::QueueSample {
                timestamp_nanos: e.timestamp_ns,
                global_queue_depth: e.global_queue as usize,
            },
            TelemetryEventRef::TaskSpawn(e) => TelemetryEvent::TaskSpawn {
                timestamp_nanos: e.timestamp_ns,
                task_id: e.task_id,
                spawn_loc_id: e.spawn_loc_id,
            },
            TelemetryEventRef::TaskTerminate(e) => TelemetryEvent::TaskTerminate {
                timestamp_nanos: e.timestamp_ns,
                task_id: e.task_id,
            },
            TelemetryEventRef::CpuSample(e) => TelemetryEvent::CpuSample {
                timestamp_nanos: e.timestamp_ns,
                worker_id: e.worker_id as usize,
                tid: e.tid,
                source: e.source,
                callchain: e.callchain.iter().collect(),
            },
            TelemetryEventRef::CallframeDef(e) => {
                let loc = e.location;
                TelemetryEvent::CallframeDef {
                    address: e.address,
                    symbol: e.symbol.to_owned(),
                    location: if loc.is_empty() { None } else { Some(loc.to_owned()) },
                }
            }
            TelemetryEventRef::ThreadNameDef(e) => TelemetryEvent::ThreadNameDef {
                tid: e.tid,
                name: e.name.to_owned(),
            },
            TelemetryEventRef::WakeEvent(e) => TelemetryEvent::WakeEvent {
                timestamp_nanos: e.timestamp_ns,
                waker_task_id: e.waker_task_id,
                woken_task_id: e.woken_task_id,
                target_worker: e.target_worker,
            },
            TelemetryEventRef::SegmentMetadata(e) => TelemetryEvent::SegmentMetadata {
                entries: e.entries.iter().map(|(k, v)| (k.to_owned(), v.to_owned())).collect(),
            },
        }
    }
}

// ── Tests ───────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;
    use crate::telemetry::task_metadata::TaskId;

    fn roundtrip(event: &TelemetryEvent) -> TelemetryEvent {
        let mut enc = Encoder::new();
        register_schemas(&mut enc).unwrap();
        write_event(&mut enc, event).unwrap();
        let bytes = enc.finish();
        let events = decode_events(&bytes).unwrap();
        assert_eq!(events.len(), 1, "expected 1 event, got {}", events.len());
        events.into_iter().next().unwrap()
    }

    /// Roundtrip helper that pre-interns a string so InternedString fields decode correctly.
    fn roundtrip_with_intern(event: &TelemetryEvent, intern: &str) -> TelemetryEvent {
        let mut enc = Encoder::new();
        register_schemas(&mut enc).unwrap();
        enc.intern_string(intern).unwrap();
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
            spawn_loc_id: InternedString(0),
        };
        assert_eq!(roundtrip_with_intern(&event, "test_loc"), event);
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
    fn task_spawn_roundtrip() {
        let event = TelemetryEvent::TaskSpawn {
            timestamp_nanos: 5_000_000,
            task_id: TaskId::from_u32(99),
            spawn_loc_id: InternedString(0),
        };
        assert_eq!(roundtrip_with_intern(&event, "test_loc"), event);
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
        let loc = InternedString(0);
        let events = vec![
            TelemetryEvent::TaskSpawn {
                timestamp_nanos: 500_000,
                task_id: TaskId::from_u32(100),
                spawn_loc_id: loc,
            },
            TelemetryEvent::PollStart {
                timestamp_nanos: 1_000_000,
                worker_id: 0,
                worker_local_queue_depth: 3,
                task_id: TaskId::from_u32(100),
                spawn_loc_id: loc,
            },
            TelemetryEvent::PollEnd { timestamp_nanos: 2_000_000, worker_id: 0 },
        ];

        let mut enc = Encoder::new();
        register_schemas(&mut enc).unwrap();
        enc.intern_string("src/main.rs:10:1").unwrap();
        for e in &events {
            write_event(&mut enc, e).unwrap();
        }
        let bytes = enc.finish();
        let decoded = decode_events(&bytes).unwrap();
        assert_eq!(decoded, events);
    }
}
