use dial9_tokio_telemetry::telemetry::events::{RawEvent, TelemetryEvent};
use dial9_tokio_telemetry::telemetry::task_metadata::SpawnLocationId;
use dial9_tokio_telemetry::telemetry::writer::{TraceWriter, WriteAtomicResult};
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
/// spawn-location interning, so tests can assert on `TelemetryEvent`s.
pub struct CapturingWriter {
    events: Arc<Mutex<Vec<TelemetryEvent>>>,
    intern_map: HashMap<usize, SpawnLocationId>,
    next_id: u16,
}

impl CapturingWriter {
    pub fn new() -> (Self, Arc<Mutex<Vec<TelemetryEvent>>>) {
        let events = Arc::new(Mutex::new(Vec::new()));
        (
            Self {
                events: events.clone(),
                intern_map: HashMap::new(),
                next_id: 1,
            },
            events,
        )
    }

    fn intern(
        &mut self,
        location: &'static std::panic::Location<'static>,
    ) -> SpawnLocationId {
        let ptr = location as *const std::panic::Location<'static> as usize;
        if let Some(&id) = self.intern_map.get(&ptr) {
            return id;
        }
        let id = SpawnLocationId(self.next_id);
        self.next_id += 1;
        self.intern_map.insert(ptr, id);
        // Emit the def
        self.events.lock().unwrap().push(TelemetryEvent::SpawnLocationDef {
            id,
            location: format!(
                "{}:{}:{}",
                location.file(),
                location.line(),
                location.column()
            ),
        });
        id
    }
}

impl TraceWriter for CapturingWriter {
    fn write_raw_event(&mut self, raw: RawEvent) -> std::io::Result<()> {
        let event = match raw {
            RawEvent::PollStart {
                timestamp_nanos,
                worker_id,
                worker_local_queue_depth,
                task_id,
                location,
            } => {
                let spawn_loc_id = self.intern(location);
                TelemetryEvent::PollStart {
                    timestamp_nanos,
                    worker_id,
                    worker_local_queue_depth,
                    task_id,
                    spawn_loc_id,
                }
            }
            RawEvent::TaskSpawn {
                timestamp_nanos,
                task_id,
                location,
            } => {
                let spawn_loc_id = self.intern(location);
                TelemetryEvent::TaskSpawn {
                    timestamp_nanos,
                    task_id,
                    spawn_loc_id,
                }
            }
            RawEvent::PollEnd {
                timestamp_nanos,
                worker_id,
            } => TelemetryEvent::PollEnd {
                timestamp_nanos,
                worker_id,
            },
            RawEvent::WorkerPark {
                timestamp_nanos,
                worker_id,
                worker_local_queue_depth,
                cpu_time_nanos,
            } => TelemetryEvent::WorkerPark {
                timestamp_nanos,
                worker_id,
                worker_local_queue_depth,
                cpu_time_nanos,
            },
            RawEvent::WorkerUnpark {
                timestamp_nanos,
                worker_id,
                worker_local_queue_depth,
                cpu_time_nanos,
                sched_wait_delta_nanos,
            } => TelemetryEvent::WorkerUnpark {
                timestamp_nanos,
                worker_id,
                worker_local_queue_depth,
                cpu_time_nanos,
                sched_wait_delta_nanos,
            },
            RawEvent::QueueSample {
                timestamp_nanos,
                global_queue_depth,
            } => TelemetryEvent::QueueSample {
                timestamp_nanos,
                global_queue_depth,
            },
            RawEvent::TaskTerminate {
                timestamp_nanos,
                task_id,
            } => TelemetryEvent::TaskTerminate {
                timestamp_nanos,
                task_id,
            },
            RawEvent::WakeEvent {
                timestamp_nanos,
                waker_task_id,
                woken_task_id,
                target_worker,
            } => TelemetryEvent::WakeEvent {
                timestamp_nanos,
                waker_task_id,
                woken_task_id,
                target_worker,
            },
        };
        self.events.lock().unwrap().push(event);
        Ok(())
    }

    fn write_event(&mut self, event: &TelemetryEvent) -> std::io::Result<()> {
        self.events.lock().unwrap().push(event.clone());
        Ok(())
    }

    fn write_atomic(&mut self, events: &[TelemetryEvent]) -> std::io::Result<WriteAtomicResult> {
        self.events.lock().unwrap().extend_from_slice(events);
        Ok(WriteAtomicResult::Written)
    }

    fn flush(&mut self) -> std::io::Result<()> {
        Ok(())
    }
}
