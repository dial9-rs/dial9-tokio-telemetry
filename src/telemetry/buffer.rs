use crate::telemetry::events::TelemetryEvent;
use std::cell::RefCell;

const BUFFER_CAPACITY: usize = 1024;

pub struct ThreadLocalBuffer {
    events: Vec<TelemetryEvent>,
}

impl Default for ThreadLocalBuffer {
    fn default() -> Self {
        Self::new()
    }
}

impl ThreadLocalBuffer {
    pub fn new() -> Self {
        Self {
            events: Vec::with_capacity(BUFFER_CAPACITY),
        }
    }

    pub fn record_event(&mut self, event: TelemetryEvent) {
        self.events.push(event);
    }

    pub fn should_flush(&self) -> bool {
        self.events.len() >= BUFFER_CAPACITY
    }

    pub fn flush(&mut self) -> Vec<TelemetryEvent> {
        std::mem::replace(&mut self.events, Vec::with_capacity(BUFFER_CAPACITY))
    }
}

thread_local! {
    pub static BUFFER: RefCell<ThreadLocalBuffer> = RefCell::new(ThreadLocalBuffer::new());
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::telemetry::events::{EventType, MetricsSnapshot};

    #[test]
    fn test_buffer_creation() {
        let buffer = ThreadLocalBuffer::new();
        assert_eq!(buffer.events.len(), 0);
        assert_eq!(buffer.events.capacity(), BUFFER_CAPACITY);
    }

    #[test]
    fn test_record_event() {
        let mut buffer = ThreadLocalBuffer::new();
        let metrics = MetricsSnapshot {
            timestamp_nanos: 1000,
            worker_id: 0,
            global_queue_depth: 5,
            worker_local_queue_depth: 2,
            cpu_time_nanos: 0,
            sched_wait_delta_nanos: 0,
        };
        let event = TelemetryEvent::new(EventType::PollStart, metrics);
        buffer.record_event(event);
        assert_eq!(buffer.events.len(), 1);
    }

    #[test]
    fn test_should_flush() {
        let mut buffer = ThreadLocalBuffer::new();
        assert!(!buffer.should_flush());

        let metrics = MetricsSnapshot {
            timestamp_nanos: 1000,
            worker_id: 0,
            global_queue_depth: 5,
            worker_local_queue_depth: 2,
            cpu_time_nanos: 0,
            sched_wait_delta_nanos: 0,
        };
        for _ in 0..BUFFER_CAPACITY {
            buffer.record_event(TelemetryEvent::new(EventType::PollStart, metrics));
        }
        assert!(buffer.should_flush());
    }

    #[test]
    fn test_flush() {
        let mut buffer = ThreadLocalBuffer::new();
        let metrics = MetricsSnapshot {
            timestamp_nanos: 1000,
            worker_id: 0,
            global_queue_depth: 5,
            worker_local_queue_depth: 2,
            cpu_time_nanos: 0,
            sched_wait_delta_nanos: 0,
        };
        buffer.record_event(TelemetryEvent::new(EventType::PollStart, metrics));

        let flushed = buffer.flush();
        assert_eq!(flushed.len(), 1);
        assert_eq!(buffer.events.len(), 0);
        assert_eq!(buffer.events.capacity(), BUFFER_CAPACITY);
    }
}
