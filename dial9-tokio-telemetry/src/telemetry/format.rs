use crate::telemetry::events::{CpuSampleSource, TelemetryEvent};
use crate::telemetry::task_metadata::TaskId;
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

#[cfg(feature = "analysis")]
/// Decode a single event from its schema name and zero-copy field values.
/// Returns `None` for unknown event names.
pub(crate) fn decode_ref<'a>(
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

/// Convert a zero-copy `TelemetryEventRef` into an owned `TelemetryEvent`.
impl From<TelemetryEventRef<'_>> for TelemetryEvent {
    fn from(r: TelemetryEventRef<'_>) -> Self {
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
}
