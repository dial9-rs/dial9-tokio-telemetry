use crate::telemetry::events::{CpuSampleSource, TelemetryEvent};
use crate::telemetry::task_metadata::TaskId;
use dial9_trace_format::decoder::StringPool;
use dial9_trace_format::types::{EventEncoder, FieldType, FieldValueRef};
use dial9_trace_format::{InternedString, StackFrames, TraceEvent, TraceField};
use serde::Serialize;
use std::fmt;
use std::io::{self, Write};

// ── WorkerId newtype ────────────────────────────────────────────────────────

/// Identifies a Tokio worker thread. Wraps a `u64` encoded as a varint on the wire.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Default)]
pub struct WorkerId(pub(crate) u64);

impl WorkerId {
    /// Sentinel for events from non-worker threads.
    pub const UNKNOWN: WorkerId = WorkerId(255);
    /// Sentinel for events from tokio's blocking thread pool.
    pub const BLOCKING: WorkerId = WorkerId(254);

    pub fn as_u64(self) -> u64 {
        self.0
    }
}

impl From<usize> for WorkerId {
    fn from(v: usize) -> Self {
        WorkerId(v as u64)
    }
}

impl From<u8> for WorkerId {
    fn from(v: u8) -> Self {
        WorkerId(v as u64)
    }
}

impl fmt::Display for WorkerId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}", self.0)
    }
}

// ── dial9-trace-format: TraceField impls ────────────────────────────────────

impl TraceField for TaskId {
    type Ref<'a> = TaskId;
    fn field_type() -> FieldType {
        FieldType::Varint
    }
    fn encode<W: Write>(&self, enc: &mut EventEncoder<'_, W>) -> io::Result<()> {
        enc.write_u64(self.0)
    }
    fn decode_ref<'a>(val: &FieldValueRef<'a>) -> Option<Self::Ref<'a>> {
        match val {
            FieldValueRef::Varint(v) => Some(TaskId(*v)),
            _ => None,
        }
    }
}

impl TraceField for CpuSampleSource {
    type Ref<'a> = CpuSampleSource;
    fn field_type() -> FieldType {
        FieldType::U8
    }
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

impl TraceField for WorkerId {
    type Ref<'a> = WorkerId;

    fn field_type() -> FieldType {
        FieldType::Varint
    }

    fn encode<W: Write>(&self, enc: &mut EventEncoder<'_, W>) -> io::Result<()> {
        enc.write_u64(self.0)
    }

    fn decode_ref<'a>(val: &FieldValueRef<'a>) -> Option<Self::Ref<'a>> {
        match val {
            FieldValueRef::Varint(v) => Some(WorkerId(*v)),
            _ => None,
        }
    }
}

// ── dial9-trace-format: derive structs ──────────────────────────────────────

#[derive(TraceEvent)]
pub struct PollStartEvent {
    #[traceevent(timestamp)]
    pub timestamp_ns: u64,
    pub worker_id: WorkerId,
    pub local_queue: u8,
    pub task_id: TaskId,
    pub spawn_loc: InternedString,
}

#[derive(TraceEvent)]
pub struct PollEndEvent {
    #[traceevent(timestamp)]
    pub timestamp_ns: u64,
    pub worker_id: WorkerId,
}

#[derive(TraceEvent)]
pub struct WorkerParkEvent {
    #[traceevent(timestamp)]
    pub timestamp_ns: u64,
    pub worker_id: WorkerId,
    pub local_queue: u8,
    pub cpu_time_ns: u64,
}

#[derive(TraceEvent)]
pub struct WorkerUnparkEvent {
    #[traceevent(timestamp)]
    pub timestamp_ns: u64,
    pub worker_id: WorkerId,
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
    pub spawn_loc: InternedString,
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
    pub worker_id: WorkerId,
    pub tid: u32,
    pub source: CpuSampleSource,
    pub thread_name: InternedString,
    pub callchain: StackFrames,
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
    #[traceevent(timestamp)]
    pub timestamp_ns: u64,
    pub entries: Vec<(String, String)>,
}

// ── dial9-trace-format: decode ──────────────────────────────────────────────

/// Decode all events from a `dial9-trace-format` byte slice into `TelemetryEvent`s.
///
/// Resolves `InternedString` fields (e.g. `CpuSample.thread_name`) via the
/// decoder's string pool while it is still valid for each batch.
pub fn decode_events(data: &[u8]) -> io::Result<Vec<TelemetryEvent>> {
    use dial9_trace_format::decoder::Decoder;

    let mut dec = Decoder::new(data)
        .ok_or_else(|| io::Error::new(io::ErrorKind::InvalidData, "invalid trace header"))?;
    let mut events = Vec::new();

    dec.for_each_event(|ev| {
        if let Some(r) = decode_ref(ev.name, ev.timestamp_ns, ev.fields) {
            events.push(to_owned_event(r, ev.string_pool));
        }
    })
    .map_err(|e| io::Error::new(io::ErrorKind::InvalidData, e.to_string()))?;

    Ok(events)
}

/// Decode all events from a byte slice into zero-copy [`TelemetryEventRef`]s.
pub fn decode_events_ref(data: &[u8]) -> io::Result<Vec<TelemetryEventRef<'_>>> {
    use dial9_trace_format::decoder::Decoder;

    let mut dec = Decoder::new(data)
        .ok_or_else(|| io::Error::new(io::ErrorKind::InvalidData, "invalid trace header"))?;
    let mut events = Vec::new();

    dec.for_each_event(|ev| {
        if let Some(ev) = decode_ref(ev.name, ev.timestamp_ns, ev.fields) {
            events.push(ev);
        }
    })
    .map_err(|e| io::Error::new(io::ErrorKind::InvalidData, e.to_string()))?;

    Ok(events)
}

/// Zero-copy enum of all telemetry event types. Each variant wraps the
/// derive-generated `*EventRef<'a>` that borrows directly from the decode buffer.
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
            Self::SegmentMetadata(e) => Some(e.timestamp_ns),
        }
    }
}

/// Decode a single event from its schema name and zero-copy field values.
/// Returns `None` for unknown event names.
pub fn decode_ref<'a>(
    name: &str,
    timestamp_ns: Option<u64>,
    fields: &[FieldValueRef<'a>],
) -> Option<TelemetryEventRef<'a>> {
    use dial9_trace_format::TraceEvent as _;
    Some(match name {
        "PollStartEvent" => {
            TelemetryEventRef::PollStart(PollStartEvent::decode(timestamp_ns, fields)?)
        }
        "PollEndEvent" => TelemetryEventRef::PollEnd(PollEndEvent::decode(timestamp_ns, fields)?),
        "WorkerParkEvent" => {
            TelemetryEventRef::WorkerPark(WorkerParkEvent::decode(timestamp_ns, fields)?)
        }
        "WorkerUnparkEvent" => {
            TelemetryEventRef::WorkerUnpark(WorkerUnparkEvent::decode(timestamp_ns, fields)?)
        }
        "QueueSampleEvent" => {
            TelemetryEventRef::QueueSample(QueueSampleEvent::decode(timestamp_ns, fields)?)
        }
        "TaskSpawnEvent" => {
            TelemetryEventRef::TaskSpawn(TaskSpawnEvent::decode(timestamp_ns, fields)?)
        }
        "TaskTerminateEvent" => {
            TelemetryEventRef::TaskTerminate(TaskTerminateEvent::decode(timestamp_ns, fields)?)
        }
        "CpuSampleEvent" => {
            TelemetryEventRef::CpuSample(CpuSampleEvent::decode(timestamp_ns, fields)?)
        }
        "WakeEventEvent" => {
            TelemetryEventRef::WakeEvent(WakeEventEvent::decode(timestamp_ns, fields)?)
        }
        "SegmentMetadataEvent" => {
            TelemetryEventRef::SegmentMetadata(SegmentMetadataEvent::decode(timestamp_ns, fields)?)
        }
        _ => return None,
    })
}

/// Resolve an interned thread name from the string pool, filtering out the
/// sentinel `"<no thread name>"` placeholder.
fn resolve_thread_name(pool: &StringPool, interned: InternedString) -> Option<String> {
    pool.get(interned)
        .filter(|n| *n != "<no thread name>")
        .map(|n| n.to_string())
}

/// Convert a zero-copy `TelemetryEventRef` into an owned `TelemetryEvent`,
/// resolving any `InternedString` fields (e.g. `thread_name`) via the
/// string pool that was active when the event was decoded.
pub(crate) fn to_owned_event(r: TelemetryEventRef<'_>, pool: &StringPool) -> TelemetryEvent {
    match r {
        TelemetryEventRef::PollStart(e) => TelemetryEvent::PollStart {
            timestamp_nanos: e.timestamp_ns,
            worker_id: e.worker_id,
            worker_local_queue_depth: e.local_queue as usize,
            task_id: e.task_id,
            spawn_loc: e.spawn_loc,
        },
        TelemetryEventRef::PollEnd(e) => TelemetryEvent::PollEnd {
            timestamp_nanos: e.timestamp_ns,
            worker_id: e.worker_id,
        },
        TelemetryEventRef::WorkerPark(e) => TelemetryEvent::WorkerPark {
            timestamp_nanos: e.timestamp_ns,
            worker_id: e.worker_id,
            worker_local_queue_depth: e.local_queue as usize,
            cpu_time_nanos: e.cpu_time_ns,
        },
        TelemetryEventRef::WorkerUnpark(e) => TelemetryEvent::WorkerUnpark {
            timestamp_nanos: e.timestamp_ns,
            worker_id: e.worker_id,
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
            spawn_loc: e.spawn_loc,
        },
        TelemetryEventRef::TaskTerminate(e) => TelemetryEvent::TaskTerminate {
            timestamp_nanos: e.timestamp_ns,
            task_id: e.task_id,
        },
        TelemetryEventRef::CpuSample(e) => TelemetryEvent::CpuSample {
            timestamp_nanos: e.timestamp_ns,
            worker_id: e.worker_id,
            tid: e.tid,
            thread_name: resolve_thread_name(pool, e.thread_name),
            source: e.source,
            callchain: e.callchain.iter().collect(),
        },
        TelemetryEventRef::WakeEvent(e) => TelemetryEvent::WakeEvent {
            timestamp_nanos: e.timestamp_ns,
            waker_task_id: e.waker_task_id,
            woken_task_id: e.woken_task_id,
            target_worker: e.target_worker,
        },
        TelemetryEventRef::SegmentMetadata(e) => TelemetryEvent::SegmentMetadata {
            timestamp_nanos: e.timestamp_ns,
            entries: e
                .entries
                .iter()
                .map(|(k, v)| (k.to_owned(), v.to_owned()))
                .collect(),
        },
    }
}
