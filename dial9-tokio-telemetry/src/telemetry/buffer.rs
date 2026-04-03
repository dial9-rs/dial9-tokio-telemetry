//! `ThreadLocalBuffer` is the entrypoint for almost all dial9 events
//!
//! The TL buffer is created lazily the first time an event is sent. Events are encoded directly
//! into a thread-local `Encoder<Vec<u8>>` and flushed to the central collector when the encoded
//! batch reaches the configured batch size (default 1 MB).
use crate::telemetry::collector::CentralCollector;
use crate::telemetry::events::RawEvent;
use crate::telemetry::format::*;
use dial9_trace_format::InternedString;
use dial9_trace_format::encoder::Encoder;
use std::cell::RefCell;
use std::collections::HashMap;
use std::panic::Location;
use std::sync::Arc;

/// Default maximum encoded batch size before flushing (~1MB).
const DEFAULT_BATCH_SIZE: usize = 1023 * 1024;

pub(crate) struct ThreadLocalBuffer {
    encoder: Encoder<Vec<u8>>,
    event_count: usize,
    batch_size: usize,
    collector: Option<Arc<CentralCollector>>,
    /// Caches `Location::to_string()` to avoid re-formatting on every event.
    /// Bounded by the number of `#[track_caller]` call sites in the program,
    /// which is fixed at compile time, so this does not grow unboundedly.
    location_cache: HashMap<&'static Location<'static>, String>,
}

impl Default for ThreadLocalBuffer {
    fn default() -> Self {
        Self::new()
    }
}

impl ThreadLocalBuffer {
    fn new() -> Self {
        Self::with_batch_size(DEFAULT_BATCH_SIZE)
    }

    fn with_batch_size(batch_size: usize) -> Self {
        Self {
            // Allocate 1KB extra headroom so typical events never trigger a realloc.
            encoder: Encoder::new_to(Vec::with_capacity(batch_size + 1024))
                .expect("Vec::write_all cannot fail"),
            event_count: 0,
            batch_size,
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

    fn record_event(&mut self, event: &RawEvent) {
        self.encode_event(event);
        self.event_count += 1;
    }

    /// Encode a single event into a self-contained batch (header + event).
    /// Used by tests that need to write individual events through the batch API.
    #[cfg(test)]
    pub(crate) fn encode_single(event: &RawEvent) -> Vec<u8> {
        let mut buf = Self::with_batch_size(1024);
        buf.encode_event(event);
        buf.flush().encoded_bytes
    }

    fn should_flush(&self) -> bool {
        self.encoder.bytes_written() as usize >= self.batch_size
    }

    fn flush(&mut self) -> crate::telemetry::collector::Batch {
        let event_count = self.event_count as u64;
        let encoded_bytes = self
            .encoder
            .reset_to_infallible(Vec::with_capacity(self.batch_size));
        self.event_count = 0;
        crate::telemetry::collector::Batch {
            encoded_bytes,
            event_count,
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
        buf.record_event(&event);
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
        assert_eq!(buffer.event_count, 0);
        assert_eq!(buffer.batch_size, DEFAULT_BATCH_SIZE);
    }

    #[test]
    fn test_record_event() {
        let mut buffer = ThreadLocalBuffer::new();
        buffer.record_event(&poll_end_event());
        assert_eq!(buffer.event_count, 1);
        assert!(buffer.encoder.bytes_written() > 0);
    }

    #[test]
    fn test_should_flush_respects_batch_size() {
        // Use a tiny batch size so a single event triggers flush.
        let mut buffer = ThreadLocalBuffer::with_batch_size(1);
        assert!(!buffer.should_flush());
        buffer.record_event(&poll_end_event());
        assert!(buffer.should_flush());
    }

    #[test]
    fn test_should_flush_default_batch_size() {
        let mut buffer = ThreadLocalBuffer::new();
        assert!(!buffer.should_flush());
        buffer.record_event(&poll_end_event());
        // A single small event should not exceed 1 MB.
        assert!(!buffer.should_flush());
    }

    #[test]
    fn test_flush() {
        let mut buffer = ThreadLocalBuffer::new();
        buffer.record_event(&poll_end_event());
        let batch = buffer.flush();
        assert!(!batch.encoded_bytes.is_empty());
        assert_eq!(buffer.event_count, 0);
    }
}
