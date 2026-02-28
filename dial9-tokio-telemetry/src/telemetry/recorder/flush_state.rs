use crate::telemetry::events::{RawEvent, TelemetryEvent};
use crate::telemetry::task_metadata::SpawnLocationId;
use smallvec::SmallVec;
use std::collections::{HashMap, HashSet};
use std::panic::Location;

/// Flush-thread state for interning spawn locations and tracking per-file emissions.
pub(super) struct FlushState {
    /// Location pointer (as usize) → SpawnLocationId. Only touched by flush thread.
    intern_map: HashMap<usize, SpawnLocationId>,
    /// SpawnLocationId → location string.
    intern_strings: Vec<String>,
    /// Which SpawnLocationIds have been emitted as SpawnLocationDef in the current file.
    pub(super) emitted_this_file: HashSet<SpawnLocationId>,
    next_id: u16,
}

impl FlushState {
    pub(super) fn new() -> Self {
        let intern_strings = vec!["<unknown>".to_string()];
        Self {
            intern_map: HashMap::new(),
            intern_strings,
            emitted_this_file: HashSet::new(),
            next_id: 1,
        }
    }

    /// Intern a location, returning its SpawnLocationId.
    pub(super) fn intern(&mut self, location: &'static Location<'static>) -> SpawnLocationId {
        let ptr = location as *const Location<'static> as usize;
        if let Some(&id) = self.intern_map.get(&ptr) {
            return id;
        }
        let id = SpawnLocationId(self.next_id);
        self.next_id += 1;
        self.intern_map.insert(ptr, id);
        self.intern_strings.push(format!(
            "{}:{}:{}",
            location.file(),
            location.line(),
            location.column()
        ));
        id
    }

    /// If this id hasn't been emitted in the current file, push its def into `defs`.
    pub(super) fn collect_def(
        &mut self,
        id: SpawnLocationId,
        defs: &mut SmallVec<[TelemetryEvent; 3]>,
    ) {
        if self.emitted_this_file.insert(id) {
            let loc = self.intern_strings[id.as_u16() as usize].clone();
            defs.push(TelemetryEvent::SpawnLocationDef { id, location: loc });
        }
    }

    /// Resolve a RawEvent into a SmallVec of wire events: defs first, then the event itself.
    pub(super) fn resolve(&mut self, raw: RawEvent) -> SmallVec<[TelemetryEvent; 3]> {
        let mut events = SmallVec::new();
        match raw {
            RawEvent::TaskSpawn { task_id, location } => {
                let spawn_loc_id = self.intern(location);
                self.collect_def(spawn_loc_id, &mut events);
                events.push(TelemetryEvent::TaskSpawn {
                    task_id,
                    spawn_loc_id,
                });
            }
            RawEvent::PollStart {
                timestamp_nanos,
                worker_id,
                worker_local_queue_depth,
                task_id,
                location,
            } => {
                let spawn_loc_id = self.intern(location);
                self.collect_def(spawn_loc_id, &mut events);
                events.push(TelemetryEvent::PollStart {
                    timestamp_nanos,
                    worker_id,
                    worker_local_queue_depth,
                    task_id,
                    spawn_loc_id,
                });
            }
            RawEvent::PollEnd {
                timestamp_nanos,
                worker_id,
            } => {
                events.push(TelemetryEvent::PollEnd {
                    timestamp_nanos,
                    worker_id,
                });
            }
            RawEvent::WorkerPark {
                timestamp_nanos,
                worker_id,
                worker_local_queue_depth,
                cpu_time_nanos,
            } => {
                events.push(TelemetryEvent::WorkerPark {
                    timestamp_nanos,
                    worker_id,
                    worker_local_queue_depth,
                    cpu_time_nanos,
                });
            }
            RawEvent::WorkerUnpark {
                timestamp_nanos,
                worker_id,
                worker_local_queue_depth,
                cpu_time_nanos,
                sched_wait_delta_nanos,
            } => {
                events.push(TelemetryEvent::WorkerUnpark {
                    timestamp_nanos,
                    worker_id,
                    worker_local_queue_depth,
                    cpu_time_nanos,
                    sched_wait_delta_nanos,
                });
            }
            RawEvent::QueueSample {
                timestamp_nanos,
                global_queue_depth,
            } => {
                events.push(TelemetryEvent::QueueSample {
                    timestamp_nanos,
                    global_queue_depth,
                });
            }
            RawEvent::WakeEvent {
                timestamp_nanos,
                waker_task_id,
                woken_task_id,
                target_worker,
            } => {
                events.push(TelemetryEvent::WakeEvent {
                    timestamp_nanos,
                    waker_task_id,
                    woken_task_id,
                    target_worker,
                });
            }
        }
        events
    }

    /// Called on file rotation — next reference to any id will re-emit its def.
    pub(super) fn on_rotate(&mut self) {
        self.emitted_this_file.clear();
    }
}
