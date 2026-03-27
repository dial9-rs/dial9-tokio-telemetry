use dial9_tokio_telemetry::telemetry::events::{CpuSampleData, CpuSampleSource, RawEvent};
use dial9_tokio_telemetry::telemetry::writer::TraceWriter;
use dial9_trace_format::decoder::Decoder;
use dial9_trace_format::types::FieldValueRef;
use std::sync::{Arc, Mutex};

/// A [`TraceWriter`] that accumulates all events into a shared `Vec`.
///
/// Encoded batches are decoded back into `RawEvent` variants so that
/// tests can inspect them uniformly regardless of the encoding path.
pub struct CapturingWriter {
    events: Arc<Mutex<Vec<RawEvent>>>,
}

impl CapturingWriter {
    pub fn new() -> (Self, Arc<Mutex<Vec<RawEvent>>>) {
        let events = Arc::new(Mutex::new(Vec::new()));
        (
            Self {
                events: events.clone(),
            },
            events,
        )
    }
}

fn varint(fields: &[FieldValueRef<'_>], idx: usize) -> u64 {
    match fields.get(idx) {
        Some(FieldValueRef::Varint(v)) => *v,
        _ => 0,
    }
}

impl TraceWriter for CapturingWriter {
    fn write_encoded_batch(&mut self, bytes: &[u8]) -> std::io::Result<()> {
        if let Some(mut decoder) = Decoder::new(bytes) {
            let mut evs = self.events.lock().unwrap();
            let _ = decoder.for_each_event(|ev| {
                use dial9_tokio_telemetry::telemetry::format::WorkerId;
                let ts = ev.timestamp_ns.unwrap_or(0);
                let f = ev.fields;
                let raw = match ev.name {
                    "PollStartEvent" => Some(RawEvent::PollStart {
                        timestamp_nanos: ts,
                        worker_id: WorkerId::from(varint(f, 0) as usize),
                        worker_local_queue_depth: varint(f, 1) as usize,
                        task_id: Default::default(),
                        location: std::panic::Location::caller(),
                    }),
                    "PollEndEvent" => Some(RawEvent::PollEnd {
                        timestamp_nanos: ts,
                        worker_id: WorkerId::from(varint(f, 0) as usize),
                    }),
                    "WorkerParkEvent" => Some(RawEvent::WorkerPark {
                        timestamp_nanos: ts,
                        worker_id: WorkerId::from(varint(f, 0) as usize),
                        worker_local_queue_depth: varint(f, 1) as usize,
                        cpu_time_nanos: varint(f, 2),
                    }),
                    "WorkerUnparkEvent" => Some(RawEvent::WorkerUnpark {
                        timestamp_nanos: ts,
                        worker_id: WorkerId::from(varint(f, 0) as usize),
                        worker_local_queue_depth: varint(f, 1) as usize,
                        cpu_time_nanos: varint(f, 2),
                        sched_wait_delta_nanos: varint(f, 3),
                    }),
                    "TaskSpawnEvent" => Some(RawEvent::TaskSpawn {
                        timestamp_nanos: ts,
                        task_id: Default::default(),
                        location: std::panic::Location::caller(),
                    }),
                    "TaskTerminateEvent" => Some(RawEvent::TaskTerminate {
                        timestamp_nanos: ts,
                        task_id: Default::default(),
                    }),
                    "CpuSampleEvent" => {
                        let worker_id = WorkerId::from(varint(f, 0) as usize);
                        let tid = varint(f, 1) as u32;
                        let source = CpuSampleSource::from_u8(varint(f, 2) as u8);
                        let callchain = match f.get(4) {
                            Some(FieldValueRef::StackFrames(frames)) => {
                                frames.iter().collect()
                            }
                            _ => vec![],
                        };
                        Some(RawEvent::CpuSample(Box::new(CpuSampleData {
                            timestamp_nanos: ts,
                            worker_id,
                            tid,
                            source,
                            thread_name: None,
                            callchain,
                        })))
                    }
                    "SegmentMetadataEvent" | "WakeEventEvent" | "QueueSampleEvent" => None,
                    other => panic!("unhandled event type in CapturingWriter: {other}"),
                };
                if let Some(e) = raw {
                    evs.push(e);
                }
            });
        }
        Ok(())
    }

    fn flush(&mut self) -> std::io::Result<()> {
        Ok(())
    }
}
