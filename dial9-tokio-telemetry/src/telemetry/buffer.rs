//! `ThreadLocalBuffer` is the entrypoint for almost all dial9 events
//!
//! The TL buffer is created lazily the first time an event is sent. Events are buffered into a fixed-size Vec (currently 1024 items)
//! before being flushed to the central collector.
use crate::telemetry::collector::CentralCollector;
use crate::telemetry::events::RawEvent;
use std::cell::RefCell;
use std::sync::Arc;

const BUFFER_CAPACITY: usize = 1024;

pub(crate) struct ThreadLocalBuffer {
    pub(crate) events: Vec<RawEvent>,
    collector: Option<Arc<CentralCollector>>,
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
            collector: None,
        }
    }

    /// Ensure the collector reference is set. Called on every record_event;
    /// only the first call per thread actually stores the Arc.
    fn set_collector(&mut self, collector: &Arc<CentralCollector>) {
        if self.collector.is_none() {
            self.collector = Some(Arc::clone(collector));
        }
    }

    fn record_event(&mut self, event: RawEvent) {
        self.events.push(event);
    }

    fn should_flush(&self) -> bool {
        self.events.len() >= BUFFER_CAPACITY
    }

    fn flush(&mut self) -> Vec<RawEvent> {
        std::mem::replace(&mut self.events, Vec::with_capacity(BUFFER_CAPACITY))
    }
}

impl Drop for ThreadLocalBuffer {
    fn drop(&mut self) {
        if !self.events.is_empty() {
            if let Some(collector) = self.collector.take() {
                collector.accept_flush(std::mem::take(&mut self.events));
            } else {
                tracing::warn!(
                    "dial9-tokio-telemetry: dropping {} unflushed events (no collector registered on this thread)",
                    self.events.len()
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
        let events = buf.flush();
        if !events.is_empty() {
            collector.accept_flush(events);
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
