use crate::telemetry::events::CpuSampleSource;
#[cfg(any(feature = "analysis", test))]
use crate::telemetry::events::TelemetryEvent;
use crate::telemetry::task_metadata::TaskId;
#[cfg(any(feature = "analysis", test))]
use dial9_trace_format::decoder::{StackPool, StringPool};
#[cfg(any(feature = "analysis", test))]
use dial9_trace_format::schema::SchemaEntry;
use dial9_trace_format::types::{EventEncoder, FieldType, FieldValueRef};
use dial9_trace_format::{InternedStackFrames, InternedString, TraceEvent, TraceField};
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

    /// Returns the raw `u64` value.
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

/// Wire-format event for a task poll start.
#[derive(Debug, TraceEvent)]
pub struct PollStartEvent {
    /// Timestamp in nanoseconds.
    #[traceevent(timestamp)]
    pub timestamp_ns: u64,
    /// Worker thread index.
    pub worker_id: WorkerId,
    /// Local queue depth (capped to u8).
    pub local_queue: u8,
    /// Task being polled.
    pub task_id: TaskId,
    /// Interned spawn location.
    pub spawn_loc: InternedString,
}

/// Wire-format event for a task poll end.
#[derive(Debug, TraceEvent)]
pub struct PollEndEvent {
    /// Timestamp in nanoseconds.
    #[traceevent(timestamp)]
    pub timestamp_ns: u64,
    /// Worker thread index.
    pub worker_id: WorkerId,
}

/// Wire-format event for a worker park.
#[derive(Debug, TraceEvent)]
pub struct WorkerParkEvent {
    /// Timestamp in nanoseconds.
    #[traceevent(timestamp)]
    pub timestamp_ns: u64,
    /// Worker thread index.
    pub worker_id: WorkerId,
    /// Local queue depth (capped to u8).
    pub local_queue: u8,
    /// Thread CPU time in nanoseconds.
    pub cpu_time_ns: u64,
    /// OS thread ID of the parking thread. On Linux/Android, the result of gettid();
    /// on other platforms, a synthetic per-process counter — see `events::current_tid`.
    pub tid: u32,
}

/// Wire-format event for a worker unpark.
#[derive(Debug, TraceEvent)]
pub struct WorkerUnparkEvent {
    /// Timestamp in nanoseconds.
    #[traceevent(timestamp)]
    pub timestamp_ns: u64,
    /// Worker thread index.
    pub worker_id: WorkerId,
    /// Local queue depth (capped to u8).
    pub local_queue: u8,
    /// Thread CPU time in nanoseconds.
    pub cpu_time_ns: u64,
    /// Scheduling wait delta in nanoseconds.
    pub sched_wait_ns: u64,
    /// OS thread ID of the unparking thread. On Linux/Android, the result of gettid();
    /// on other platforms, a synthetic per-process counter — see `events::current_tid`.
    pub tid: u32,
}

#[derive(TraceEvent)]
pub(crate) struct QueueSampleEvent {
    #[traceevent(timestamp)]
    pub timestamp_ns: u64,
    pub global_queue: u8,
}

/// Wire-format event for a task spawn.
#[derive(Debug, TraceEvent)]
pub struct TaskSpawnEvent {
    /// Timestamp in nanoseconds.
    #[traceevent(timestamp)]
    pub timestamp_ns: u64,
    /// Spawned task identifier.
    pub task_id: TaskId,
    /// Interned spawn location.
    pub spawn_loc: InternedString,
    /// Whether this spawn was instrumented (via `TelemetryHandle::spawn`).
    pub instrumented: bool,
}

#[derive(TraceEvent)]
pub(crate) struct TaskTerminateEvent {
    #[traceevent(timestamp)]
    pub timestamp_ns: u64,
    pub task_id: TaskId,
}

#[derive(TraceEvent)]
pub(crate) struct CpuSampleEvent {
    #[traceevent(timestamp)]
    pub timestamp_ns: u64,
    pub worker_id: WorkerId,
    pub tid: u32,
    pub source: CpuSampleSource,
    pub thread_name: Option<InternedString>,
    pub callchain: InternedStackFrames,
    /// CPU the sample was taken on, if the backend could determine it.
    ///
    /// Widened to `u64` on the wire so the field encodes as `OptionalVarint`:
    /// 1 byte when absent, typically 2 bytes (tag + small-varint) when present.
    pub cpu: Option<u64>,
}

/// Wire-format event for a task dump: async backtrace captured at a yield point
/// after the task stayed idle past the configured threshold.
#[derive(TraceEvent)]
pub(crate) struct TaskDumpEvent {
    #[traceevent(timestamp)]
    pub timestamp_ns: u64,
    pub task_id: TaskId,
    pub callchain: InternedStackFrames,
}

/// Wire-format event for a sampled memory allocation.
///
/// Emitted from the consolidator (flush thread) for allocations that tripped
/// the geometric sampling counter. The sampling rate that produced this event
/// lives in the segment metadata, not on each event.
#[derive(Debug, TraceEvent)]
#[cfg_attr(not(feature = "unstable-events"), non_exhaustive)]
pub struct AllocEvent {
    /// Wall-clock timestamp in nanoseconds (monotonic).
    #[traceevent(timestamp)]
    pub timestamp_ns: u64,
    /// OS thread ID of the allocating thread. Same source as `WorkerParkEvent.tid`
    /// and `CpuSampleEvent.tid`. Use this to join against worker park/unpark
    /// history to recover worker_id when the allocation happened on a tokio
    /// worker thread.
    pub tid: u32,
    /// Allocation size in bytes. The actual size requested by the allocating
    /// code; the underlying allocator may have rounded up, but that's not
    /// recorded here.
    pub size: u64,
    /// Returned pointer. Only meaningful when liveset tracking is on; otherwise 0.
    /// Always present so the schema is stable across track_liveset on/off.
    pub addr: u64,
    /// Stack at the allocation site. Frame 0 is the most-recent caller.
    pub callchain: InternedStackFrames,
}

/// Wire-format event for a deallocation paired with a previously-sampled
/// `AllocEvent`. Only emitted when liveset tracking is on.
///
/// `size` and `alloc_timestamp_ns` are denormalized from the matching
/// `AllocEvent` so the free stays analytically useful when the corresponding
/// `AllocEvent` has been evicted by trace rotation. See design §3
/// "Why denormalize size and alloc_timestamp_ns?" for the rationale.
#[derive(Debug, TraceEvent)]
#[cfg_attr(not(feature = "unstable-events"), non_exhaustive)]
pub struct FreeEvent {
    /// Wall-clock timestamp in nanoseconds (monotonic) of the free.
    #[traceevent(timestamp)]
    pub timestamp_ns: u64,
    /// OS thread ID of the freeing thread.
    pub tid: u32,
    /// Pointer that was freed. Matches a previously-seen `AllocEvent.addr`.
    pub addr: u64,
    /// Size of the allocation being freed. Denormalized from the matching
    /// `AllocEvent` for rotation robustness.
    pub size: u64,
    /// Monotonic-ns timestamp of the original `AllocEvent`. Allows leak
    /// analysis to bucket frees by generation without needing the
    /// `AllocEvent` in the same (unrotated) trace.
    pub alloc_timestamp_ns: u64,
}

/// Wire-format event for a wake notification.
#[derive(Debug, TraceEvent)]
pub struct WakeEventEvent {
    /// Timestamp in nanoseconds.
    #[traceevent(timestamp)]
    pub timestamp_ns: u64,
    /// Task that issued the wake.
    pub waker_task_id: TaskId,
    /// Task that was woken.
    pub woken_task_id: TaskId,
    /// Worker index that issued the wake (255 = unknown).
    pub target_worker: u8,
}

#[derive(TraceEvent)]
pub(crate) struct SegmentMetadataEvent {
    #[traceevent(timestamp)]
    pub timestamp_ns: u64,
    pub entries: Vec<(String, String)>,
}

/// Clock-correlation anchor. `timestamp_ns` (monotonic) and `realtime_ns`
/// (nanoseconds since Unix epoch) are captured at the same instant via
/// [`clock_pair`], so offline consumers can recover wall clock from the
/// monotonic event stream.
///
/// [`clock_pair`]: crate::telemetry::events::clock_pair
#[derive(TraceEvent)]
pub(crate) struct ClockSyncEvent {
    #[traceevent(timestamp)]
    pub timestamp_ns: u64,
    pub realtime_ns: u64,
}

// ── dial9-trace-format: decode ──────────────────────────────────────────────

/// Decode all events from a `dial9-trace-format` byte slice into `TelemetryEvent`s.
///
/// Resolves `InternedString` fields (e.g. `CpuSample.thread_name`) via the
/// decoder's string pool while it is still valid for each batch.
#[cfg(any(feature = "analysis", test))]
pub fn decode_events(data: &[u8]) -> io::Result<Vec<TelemetryEvent>> {
    use dial9_trace_format::decoder::Decoder;

    let mut dec = Decoder::new(data)
        .ok_or_else(|| io::Error::new(io::ErrorKind::InvalidData, "invalid trace header"))?;
    let mut events = Vec::new();

    dec.for_each_event(|ev| {
        if let Some(r) = decode_ref(ev.name, ev.timestamp_ns, ev.fields, ev.schema) {
            events.push(to_owned_event(r, ev.string_pool, ev.stack_pool));
        }
    })
    .map_err(|e| io::Error::new(io::ErrorKind::InvalidData, e.to_string()))?;

    Ok(events)
}

/// Zero-copy enum of all telemetry event types. Each variant wraps the
/// derive-generated `*EventRef<'a>` that borrows directly from the decode buffer.
#[derive(Debug, Clone)]
#[cfg(any(feature = "analysis", test))]
pub(crate) enum TelemetryEventRef<'a> {
    PollStart(PollStartEventRef<'a>),
    PollEnd(PollEndEventRef<'a>),
    WorkerPark(WorkerParkEventRef<'a>),
    WorkerUnpark(WorkerUnparkEventRef<'a>),
    QueueSample(QueueSampleEventRef<'a>),
    TaskSpawn(TaskSpawnEventRef<'a>),
    TaskTerminate(TaskTerminateEventRef<'a>),
    CpuSample(CpuSampleEventRef<'a>),
    TaskDump(TaskDumpEventRef<'a>),
    Alloc(AllocEventRef<'a>),
    Free(FreeEventRef<'a>),
    WakeEvent(WakeEventEventRef<'a>),
    SegmentMetadata(SegmentMetadataEventRef<'a>),
    ClockSync(ClockSyncEventRef<'a>),
}

#[cfg(any(feature = "analysis", test))]
impl<'a> TelemetryEventRef<'a> {
    /// Returns the timestamp in nanoseconds, if this event type carries one.
    #[allow(dead_code)]
    pub(crate) fn timestamp_ns(&self) -> Option<u64> {
        match self {
            Self::PollStart(e) => Some(e.timestamp_ns),
            Self::PollEnd(e) => Some(e.timestamp_ns),
            Self::WorkerPark(e) => Some(e.timestamp_ns),
            Self::WorkerUnpark(e) => Some(e.timestamp_ns),
            Self::QueueSample(e) => Some(e.timestamp_ns),
            Self::TaskSpawn(e) => Some(e.timestamp_ns),
            Self::TaskTerminate(e) => Some(e.timestamp_ns),
            Self::CpuSample(e) => Some(e.timestamp_ns),
            Self::TaskDump(e) => Some(e.timestamp_ns),
            Self::Alloc(e) => Some(e.timestamp_ns),
            Self::Free(e) => Some(e.timestamp_ns),
            Self::WakeEvent(e) => Some(e.timestamp_ns),
            Self::SegmentMetadata(e) => Some(e.timestamp_ns),
            Self::ClockSync(e) => Some(e.timestamp_ns),
        }
    }
}

#[cfg(any(feature = "analysis", test))]
/// Decode a single event from its schema name and zero-copy field values.
/// Returns `None` for unknown event names.
pub(crate) fn decode_ref<'a>(
    name: &str,
    timestamp_ns: Option<u64>,
    fields: &[FieldValueRef<'a>],
    schema: &SchemaEntry,
) -> Option<TelemetryEventRef<'a>> {
    use dial9_trace_format::TraceEvent as _;
    let field_defs = schema.fields();
    Some(match name {
        "PollStartEvent" => {
            TelemetryEventRef::PollStart(PollStartEvent::decode(timestamp_ns, fields, field_defs)?)
        }
        "PollEndEvent" => {
            TelemetryEventRef::PollEnd(PollEndEvent::decode(timestamp_ns, fields, field_defs)?)
        }
        "WorkerParkEvent" => TelemetryEventRef::WorkerPark(WorkerParkEvent::decode(
            timestamp_ns,
            fields,
            field_defs,
        )?),
        "WorkerUnparkEvent" => TelemetryEventRef::WorkerUnpark(WorkerUnparkEvent::decode(
            timestamp_ns,
            fields,
            field_defs,
        )?),
        "QueueSampleEvent" => TelemetryEventRef::QueueSample(QueueSampleEvent::decode(
            timestamp_ns,
            fields,
            field_defs,
        )?),
        "TaskSpawnEvent" => {
            TelemetryEventRef::TaskSpawn(TaskSpawnEvent::decode(timestamp_ns, fields, field_defs)?)
        }
        "TaskTerminateEvent" => TelemetryEventRef::TaskTerminate(TaskTerminateEvent::decode(
            timestamp_ns,
            fields,
            field_defs,
        )?),
        "CpuSampleEvent" => {
            TelemetryEventRef::CpuSample(CpuSampleEvent::decode(timestamp_ns, fields, field_defs)?)
        }
        "TaskDumpEvent" => {
            TelemetryEventRef::TaskDump(TaskDumpEvent::decode(timestamp_ns, fields, field_defs)?)
        }
        "AllocEvent" => {
            TelemetryEventRef::Alloc(AllocEvent::decode(timestamp_ns, fields, field_defs)?)
        }
        "FreeEvent" => {
            TelemetryEventRef::Free(FreeEvent::decode(timestamp_ns, fields, field_defs)?)
        }
        "WakeEventEvent" => {
            TelemetryEventRef::WakeEvent(WakeEventEvent::decode(timestamp_ns, fields, field_defs)?)
        }
        "SegmentMetadataEvent" => TelemetryEventRef::SegmentMetadata(SegmentMetadataEvent::decode(
            timestamp_ns,
            fields,
            field_defs,
        )?),
        "ClockSyncEvent" => {
            TelemetryEventRef::ClockSync(ClockSyncEvent::decode(timestamp_ns, fields, field_defs)?)
        }
        _ => return None,
    })
}

/// Convert a zero-copy `TelemetryEventRef` into an owned `TelemetryEvent`,
/// resolving any interned fields (e.g. `InternedString` for `thread_name`) via the
/// corresponding pools that were active when the event was decoded.
#[cfg(any(feature = "analysis", test))]
pub(crate) fn to_owned_event(
    r: TelemetryEventRef<'_>,
    pool: &StringPool,
    stack_pool: &StackPool,
) -> TelemetryEvent {
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
            tid: e.tid,
        },
        TelemetryEventRef::WorkerUnpark(e) => TelemetryEvent::WorkerUnpark {
            timestamp_nanos: e.timestamp_ns,
            worker_id: e.worker_id,
            worker_local_queue_depth: e.local_queue as usize,
            cpu_time_nanos: e.cpu_time_ns,
            sched_wait_delta_nanos: e.sched_wait_ns,
            tid: e.tid,
        },
        TelemetryEventRef::QueueSample(e) => TelemetryEvent::QueueSample {
            timestamp_nanos: e.timestamp_ns,
            global_queue_depth: e.global_queue as usize,
        },
        TelemetryEventRef::TaskSpawn(e) => TelemetryEvent::TaskSpawn {
            timestamp_nanos: e.timestamp_ns,
            task_id: e.task_id,
            spawn_loc: e.spawn_loc,
            instrumented: Some(e.instrumented),
        },
        TelemetryEventRef::TaskTerminate(e) => TelemetryEvent::TaskTerminate {
            timestamp_nanos: e.timestamp_ns,
            task_id: e.task_id,
        },
        TelemetryEventRef::CpuSample(e) => TelemetryEvent::CpuSample {
            timestamp_nanos: e.timestamp_ns,
            worker_id: e.worker_id,
            tid: e.tid,
            thread_name: e
                .thread_name
                .and_then(|s| pool.get(s).map(|n| n.to_string())),
            source: e.source,
            callchain: stack_pool
                .get(e.callchain)
                .expect("stack pool entry must exist for CpuSample callchain")
                .to_vec(),
            // CPU id is varint-encoded as u64 on the wire; real CPU ids fit in u32.
            cpu: e.cpu.map(|v| v as u32),
        },
        TelemetryEventRef::TaskDump(e) => TelemetryEvent::TaskDump {
            timestamp_nanos: e.timestamp_ns,
            task_id: e.task_id,
            callchain: stack_pool
                .get(e.callchain)
                .expect("stack pool entry must exist for TaskDump callchain")
                .to_vec(),
        },
        TelemetryEventRef::Alloc(e) => TelemetryEvent::Alloc {
            timestamp_nanos: e.timestamp_ns,
            tid: e.tid,
            size: e.size,
            addr: e.addr,
            callchain: stack_pool
                .get(e.callchain)
                .expect("stack pool entry must exist for AllocEvent callchain")
                .to_vec(),
        },
        TelemetryEventRef::Free(e) => TelemetryEvent::Free {
            timestamp_nanos: e.timestamp_ns,
            tid: e.tid,
            addr: e.addr,
            size: e.size,
            alloc_timestamp_nanos: e.alloc_timestamp_ns,
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
        TelemetryEventRef::ClockSync(e) => TelemetryEvent::ClockSync {
            timestamp_nanos: e.timestamp_ns,
            realtime_nanos: e.realtime_ns,
        },
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use dial9_trace_format::encoder::Encoder;

    #[test]
    fn alloc_event_round_trip() {
        let mut enc = Encoder::new_to(Vec::new()).unwrap();
        let callchain = enc.intern_stack_frames(&[0x1000, 0x2000, 0x3000]).unwrap();
        enc.write_infallible(&AllocEvent {
            timestamp_ns: 123_456_789,
            tid: 42,
            size: 4096,
            addr: 0xDEAD_BEEF_CAFE,
            callchain,
        });
        let buf = enc.into_inner();

        let events = decode_events(&buf).unwrap();
        assert_eq!(events.len(), 1);
        match &events[0] {
            TelemetryEvent::Alloc {
                timestamp_nanos,
                tid,
                size,
                addr,
                callchain,
            } => {
                assert_eq!(*timestamp_nanos, 123_456_789);
                assert_eq!(*tid, 42);
                assert_eq!(*size, 4096);
                assert_eq!(*addr, 0xDEAD_BEEF_CAFE);
                assert_eq!(callchain, &[0x1000, 0x2000, 0x3000]);
            }
            other => panic!("expected Alloc event, got {other:?}"),
        }
    }

    #[test]
    fn free_event_round_trip() {
        let mut enc = Encoder::new_to(Vec::new()).unwrap();
        enc.write_infallible(&FreeEvent {
            timestamp_ns: 999_000_000,
            tid: 7,
            addr: 0xCAFE_BABE,
            size: 2048,
            alloc_timestamp_ns: 100_000_000,
        });
        let buf = enc.into_inner();

        let events = decode_events(&buf).unwrap();
        assert_eq!(events.len(), 1);
        match &events[0] {
            TelemetryEvent::Free {
                timestamp_nanos,
                tid,
                addr,
                size,
                alloc_timestamp_nanos,
            } => {
                assert_eq!(*timestamp_nanos, 999_000_000);
                assert_eq!(*tid, 7);
                assert_eq!(*addr, 0xCAFE_BABE);
                assert_eq!(*size, 2048);
                assert_eq!(*alloc_timestamp_nanos, 100_000_000);
            }
            other => panic!("expected Free event, got {other:?}"),
        }
    }
}
