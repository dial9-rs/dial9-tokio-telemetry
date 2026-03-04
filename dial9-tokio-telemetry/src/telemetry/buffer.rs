use crate::telemetry::events::RawEvent;
use std::cell::RefCell;

#[cfg(feature = "metrique-events")]
use crate::telemetry::metrique_serializer::serialize_entry;

const BUFFER_CAPACITY: usize = 1024;

pub struct ThreadLocalBuffer {
    events: Vec<RawEvent>,
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

    pub fn record_event(&mut self, event: RawEvent) {
        self.events.push(event);
    }

    pub fn should_flush(&self) -> bool {
        self.events.len() >= BUFFER_CAPACITY
    }

    pub fn flush(&mut self) -> Vec<RawEvent> {
        std::mem::replace(&mut self.events, Vec::with_capacity(BUFFER_CAPACITY))
    }

    #[cfg(feature = "metrique-events")]
    pub fn record_metrique_entry<E: metrique::writer::Entry>(&mut self, entry: &E, worker_id: usize, entry_name: &'static str) {
        let data = serialize_entry(entry);
        let task_id = tokio::task::try_id()
            .map(crate::telemetry::task_metadata::TaskId::from)
            .unwrap_or(crate::telemetry::task_metadata::TaskId::from_u32(0));
        self.record_event(RawEvent::MetriqueEvent {
            instant: std::time::Instant::now(),
            worker_id,
            task_id,
            entry_name,
            data,
        });
    }
}

thread_local! {
    pub static BUFFER: RefCell<ThreadLocalBuffer> = RefCell::new(ThreadLocalBuffer::new());
}

#[cfg(test)]
mod tests {
    use super::*;
    fn poll_end_event() -> RawEvent {
        RawEvent::PollEnd {
            timestamp_nanos: 1000,
            worker_id: 0,
        }
    }

    #[test]
    fn test_buffer_creation() {
        let buffer = ThreadLocalBuffer::new();
        assert_eq!(buffer.events.len(), 0);
        assert_eq!(buffer.events.capacity(), BUFFER_CAPACITY);
    }

    #[test]
    fn test_record_event() {
        let mut buffer = ThreadLocalBuffer::new();
        buffer.record_event(poll_end_event());
        assert_eq!(buffer.events.len(), 1);
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
        let flushed = buffer.flush();
        assert_eq!(flushed.len(), 1);
        assert_eq!(buffer.events.len(), 0);
        assert_eq!(buffer.events.capacity(), BUFFER_CAPACITY);
    }
}
