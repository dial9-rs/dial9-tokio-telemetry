//! `ThreadLocalBuffer` is the entrypoint for almost all dial9 events
//!
//! The TL buffer is created lazily the first time an event is sent. Events are buffered into a fixed-size Vec (currently 1024 items)
//! before being flushed to the central collector.
use crate::telemetry::collector::CentralCollector;
use crate::telemetry::events::RawEvent;
use crate::telemetry::format::*;
use dial9_trace_format::InternedString;
use dial9_trace_format::encoder::Encoder;
use std::cell::RefCell;
use std::collections::HashMap;
use std::panic::Location;
use std::sync::Arc;

const BUFFER_CAPACITY: usize = 1024;
const ESTIMATED_BATCH_SIZE: usize = 16 * 1024;

pub(crate) struct ThreadLocalBuffer {
    pub(crate) events: Vec<RawEvent>,
    encoder: Encoder<Vec<u8>>,
    event_count: usize,
    collector: Option<Arc<CentralCollector>>,
    location_cache: HashMap<&'static Location<'static>, String>,
}

impl Default for ThreadLocalBuffer {
    fn default() -> Self {
        Self::new()
    }
}

impl ThreadLocalBuffer {
    fn new() -> Self {
        Self {
            events: Vec::with_capacity(BUFFER_CAPACITY),
            encoder: Encoder::new(),
            event_count: 0,
            collector: None,
            location_cache: HashMap::new(),
        }
    }

    /// Ensure the collector reference is set. Called on every record_event;
    /// only the first call per thread actually stores the Arc.
    fn set_collector(&mut self, collector: &Arc<CentralCollector>) {
        if self.collector.is_none() {
            self.collector = Some(Arc::clone(collector));
        }
    }

    /// Returns true for event types that have been migrated to thread-local encoding.
    /// Initially returns false for all types; as each type is migrated, its match arm
    /// flips to true.
    fn should_encode(_event: &RawEvent) -> bool {
        false
    }

    fn encode_event(&mut self, event: &RawEvent) {
        match event {
            RawEvent::PollStart {
                timestamp_nanos,
                worker_id,
                worker_local_queue_depth,
                task_id,
                location,
            } => {
                let spawn_loc = self.intern_location(location);
                self.encoder.write_infallible(&PollStartEvent {
                    timestamp_ns: *timestamp_nanos,
                    worker_id: *worker_id,
                    local_queue: *worker_local_queue_depth as u8,
                    task_id: *task_id,
                    spawn_loc,
                });
            }
            RawEvent::PollEnd {
                timestamp_nanos,
                worker_id,
            } => self.encoder.write_infallible(&PollEndEvent {
                timestamp_ns: *timestamp_nanos,
                worker_id: *worker_id,
            }),
            RawEvent::WorkerPark {
                timestamp_nanos,
                worker_id,
                worker_local_queue_depth,
                cpu_time_nanos,
            } => self.encoder.write_infallible(&WorkerParkEvent {
                timestamp_ns: *timestamp_nanos,
                worker_id: *worker_id,
                local_queue: *worker_local_queue_depth as u8,
                cpu_time_ns: *cpu_time_nanos,
            }),
            RawEvent::WorkerUnpark {
                timestamp_nanos,
                worker_id,
                worker_local_queue_depth,
                cpu_time_nanos,
                sched_wait_delta_nanos,
            } => self.encoder.write_infallible(&WorkerUnparkEvent {
                timestamp_ns: *timestamp_nanos,
                worker_id: *worker_id,
                local_queue: *worker_local_queue_depth as u8,
                cpu_time_ns: *cpu_time_nanos,
                sched_wait_ns: *sched_wait_delta_nanos,
            }),
            RawEvent::QueueSample {
                timestamp_nanos,
                global_queue_depth,
            } => self.encoder.write_infallible(&QueueSampleEvent {
                timestamp_ns: *timestamp_nanos,
                global_queue: *global_queue_depth as u8,
            }),
            RawEvent::TaskSpawn {
                timestamp_nanos,
                task_id,
                location,
            } => {
                let spawn_loc = self.intern_location(location);
                self.encoder.write_infallible(&TaskSpawnEvent {
                    timestamp_ns: *timestamp_nanos,
                    task_id: *task_id,
                    spawn_loc,
                });
            }
            RawEvent::TaskTerminate {
                timestamp_nanos,
                task_id,
            } => self.encoder.write_infallible(&TaskTerminateEvent {
                timestamp_ns: *timestamp_nanos,
                task_id: *task_id,
            }),
            RawEvent::WakeEvent {
                timestamp_nanos,
                waker_task_id,
                woken_task_id,
                target_worker,
            } => self.encoder.write_infallible(&WakeEventEvent {
                timestamp_ns: *timestamp_nanos,
                waker_task_id: *waker_task_id,
                woken_task_id: *woken_task_id,
                target_worker: *target_worker,
            }),
            RawEvent::CpuSample(data) => {
                let thread_name = match &data.thread_name {
                    Some(name) => self.encoder.intern_string_infallible(name.as_str()),
                    None => self.encoder.intern_string_infallible("<no thread name>"),
                };
                self.encoder.write_infallible(&CpuSampleEvent {
                    timestamp_ns: data.timestamp_nanos,
                    worker_id: data.worker_id,
                    tid: data.tid,
                    source: data.source,
                    thread_name,
                    callchain: dial9_trace_format::StackFrames(data.callchain.clone()),
                });
            }
        }
    }

    fn intern_location(&mut self, location: &'static Location<'static>) -> InternedString {
        let s = self
            .location_cache
            .entry(location)
            .or_insert_with(|| location.to_string());
        self.encoder.intern_string_infallible(s)
    }

    fn record_event(&mut self, event: RawEvent) {
        if Self::should_encode(&event) {
            self.encode_event(&event);
        } else {
            self.events.push(event);
        }
        self.event_count += 1;
    }

    fn should_flush(&self) -> bool {
        self.event_count >= BUFFER_CAPACITY
    }

    fn flush(&mut self) -> crate::telemetry::collector::Batch {
        let raw_events = std::mem::replace(&mut self.events, Vec::with_capacity(BUFFER_CAPACITY));
        let encoded_bytes = self
            .encoder
            .reset_to(Vec::with_capacity(ESTIMATED_BATCH_SIZE));
        self.event_count = 0;
        crate::telemetry::collector::Batch {
            raw_events,
            encoded_bytes,
        }
    }
}

impl Drop for ThreadLocalBuffer {
    fn drop(&mut self) {
        if self.event_count > 0 {
            if let Some(collector) = self.collector.take() {
                collector.accept_flush(self.flush());
            } else {
                tracing::warn!(
                    "dial9-tokio-telemetry: dropping {} unflushed events (no collector registered on this thread)",
                    self.event_count
                );
            }
        }
    }
}

thread_local! {
    static BUFFER: RefCell<ThreadLocalBuffer> = RefCell::new(ThreadLocalBuffer::new());
}

/// Record an event into the current thread's buffer. If the buffer is full,
/// automatically flush the batch to `collector`.
///
/// This sets the collector on the buffer so that at some point in the future when the ThreadLocalBuffer itself is dropped, we know where to send events
pub(crate) fn record_event(event: RawEvent, collector: &Arc<CentralCollector>) {
    BUFFER.with(|buf| {
        let mut buf = buf.borrow_mut();
        buf.set_collector(collector);
        buf.record_event(event);
        if buf.should_flush() {
            collector.accept_flush(buf.flush());
        }
    });
}

/// Drain the current thread's buffer into `collector`, even if not full.
/// Used at shutdown and before flush cycles to avoid losing events.
pub(crate) fn drain_to_collector(collector: &CentralCollector) {
    BUFFER.with(|buf| {
        let mut buf = buf.borrow_mut();
        if buf.event_count > 0 {
            collector.accept_flush(buf.flush());
        }
    });
}

#[cfg(test)]
mod tests {
    use super::*;
    fn poll_end_event() -> RawEvent {
        RawEvent::PollEnd {
            timestamp_nanos: 1000,
            worker_id: crate::telemetry::format::WorkerId::from(0usize),
        }
    }

    #[test]
    fn test_buffer_creation() {
        let buffer = ThreadLocalBuffer::new();
        assert_eq!(buffer.events.len(), 0);
        assert_eq!(buffer.events.capacity(), BUFFER_CAPACITY);
        assert_eq!(buffer.event_count, 0);
    }

    #[test]
    fn test_record_event() {
        let mut buffer = ThreadLocalBuffer::new();
        buffer.record_event(poll_end_event());
        assert_eq!(buffer.events.len(), 1);
        assert_eq!(buffer.event_count, 1);
    }

    #[test]
    fn test_should_flush() {
        let mut buffer = ThreadLocalBuffer::new();
        assert!(!buffer.should_flush());
        for _ in 0..BUFFER_CAPACITY {
            buffer.record_event(poll_end_event());
        }
        assert!(buffer.should_flush());
    }

    #[test]
    fn test_flush() {
        let mut buffer = ThreadLocalBuffer::new();
        buffer.record_event(poll_end_event());
        let batch = buffer.flush();
        assert_eq!(batch.raw_events.len(), 1);
        assert_eq!(buffer.events.len(), 0);
        assert_eq!(buffer.event_count, 0);
        assert_eq!(buffer.events.capacity(), BUFFER_CAPACITY);
    }
}
