use dial9_tokio_telemetry::telemetry::events::{CpuSampleSource, RawEvent, TelemetryEvent};
use dial9_tokio_telemetry::telemetry::writer::TraceWriter;
use dial9_trace_format::InternedString;
use std::collections::HashMap;
use std::sync::{Arc, Mutex};

/// Returns true when running in CI (GitHub Actions sets CI=true).
#[allow(dead_code)]
pub fn is_ci() -> bool {
    std::env::var("CI").is_ok()
}

/// A [`TraceWriter`] that accumulates all events into a shared `Vec`.
///
/// Handles `RawEvent` → `TelemetryEvent` conversion internally with simple
/// spawn-location interning via `InternedString` pool IDs.
pub struct CapturingWriter {
    events: Arc<Mutex<Vec<TelemetryEvent>>>,
    intern_map: HashMap<usize, InternedString>,
    next_id: u32,
}

impl CapturingWriter {
    pub fn new() -> (Self, Arc<Mutex<Vec<TelemetryEvent>>>) {
        let events = Arc::new(Mutex::new(Vec::new()));
        (
            Self {
                events: events.clone(),
                intern_map: HashMap::new(),
                next_id: 0,
            },
            events,
        )
    }

    fn intern(&mut self, location: &'static std::panic::Location<'static>) -> InternedString {
        let ptr = location as *const std::panic::Location<'static> as usize;
        if let Some(&id) = self.intern_map.get(&ptr) {
            return id;
        }
        let id = InternedString(self.next_id);
        self.next_id += 1;
        self.intern_map.insert(ptr, id);
        id
    }
}

impl TraceWriter for CapturingWriter {
    fn write_raw_event(&mut self, raw: RawEvent) -> std::io::Result<()> {
        let event = match raw {
            RawEvent::PollStart {
                timestamp_nanos, worker_id, worker_local_queue_depth, task_id, location,
            } => TelemetryEvent::PollStart {
                timestamp_nanos, worker_id, worker_local_queue_depth, task_id,
                spawn_loc_id: self.intern(location),
            },
            RawEvent::TaskSpawn { timestamp_nanos, task_id, location } => {
                TelemetryEvent::TaskSpawn {
                    timestamp_nanos, task_id,
                    spawn_loc_id: self.intern(location),
                }
            }
            RawEvent::PollEnd { timestamp_nanos, worker_id } =>
                TelemetryEvent::PollEnd { timestamp_nanos, worker_id },
            RawEvent::WorkerPark { timestamp_nanos, worker_id, worker_local_queue_depth, cpu_time_nanos } =>
                TelemetryEvent::WorkerPark { timestamp_nanos, worker_id, worker_local_queue_depth, cpu_time_nanos },
            RawEvent::WorkerUnpark { timestamp_nanos, worker_id, worker_local_queue_depth, cpu_time_nanos, sched_wait_delta_nanos } =>
                TelemetryEvent::WorkerUnpark { timestamp_nanos, worker_id, worker_local_queue_depth, cpu_time_nanos, sched_wait_delta_nanos },
            RawEvent::QueueSample { timestamp_nanos, global_queue_depth } =>
                TelemetryEvent::QueueSample { timestamp_nanos, global_queue_depth },
            RawEvent::TaskTerminate { timestamp_nanos, task_id } =>
                TelemetryEvent::TaskTerminate { timestamp_nanos, task_id },
            RawEvent::WakeEvent { timestamp_nanos, waker_task_id, woken_task_id, target_worker } =>
                TelemetryEvent::WakeEvent { timestamp_nanos, waker_task_id, woken_task_id, target_worker },
        };
        self.events.lock().unwrap().push(event);
        Ok(())
    }

    fn write_cpu_sample(
        &mut self,
        timestamp_nanos: u64,
        worker_id: usize,
        tid: u32,
        source: CpuSampleSource,
        callchain: &[u64],
        thread_name: Option<&str>,
    ) -> std::io::Result<()> {
        let mut events = self.events.lock().unwrap();
        if let Some(name) = thread_name {
            events.push(TelemetryEvent::ThreadNameDef { tid, name: name.to_string() });
        }
        events.push(TelemetryEvent::CpuSample {
            timestamp_nanos,
            worker_id,
            tid,
            source,
            callchain: callchain.to_vec(),
        });
        Ok(())
    }

    fn flush(&mut self) -> std::io::Result<()> {
        Ok(())
    }
}
