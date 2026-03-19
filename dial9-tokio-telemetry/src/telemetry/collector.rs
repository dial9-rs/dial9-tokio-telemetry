use crate::telemetry::events::RawEvent;
use crossbeam_queue::ArrayQueue;
use std::sync::atomic::{AtomicUsize, Ordering};

/// Maximum number of batches (each up to 1024 events) that can be buffered.
/// Beyond this, the oldest batch is evicted — the queue acts as a ring buffer
/// so the most recent data is always preserved.
const DEFAULT_CAPACITY: usize = 1024;

pub struct CentralCollector {
    queue: ArrayQueue<Vec<RawEvent>>,
    dropped_batches: AtomicUsize,
}

impl Default for CentralCollector {
    fn default() -> Self {
        Self::new()
    }
}

impl CentralCollector {
    pub fn new() -> Self {
        Self::with_capacity(DEFAULT_CAPACITY)
    }

    pub fn with_capacity(capacity: usize) -> Self {
        Self {
            queue: ArrayQueue::new(capacity),
            dropped_batches: AtomicUsize::new(0),
        }
    }

    pub fn accept_flush(&self, buffer: Vec<RawEvent>) {
        if let Some(_evicted) = self.queue.force_push(buffer) {
            self.dropped_batches.fetch_add(1, Ordering::Relaxed);
        }
    }

    pub fn next(&self) -> Option<Vec<RawEvent>> {
        self.queue.pop()
    }

    pub fn drain(&self) -> Vec<Vec<RawEvent>> {
        let mut batches = Vec::new();
        while let Some(batch) = self.queue.pop() {
            batches.push(batch);
        }
        batches
    }

    pub fn len(&self) -> usize {
        self.queue.len()
    }

    pub fn is_empty(&self) -> bool {
        self.queue.is_empty()
    }

    /// Returns the number of batches dropped since the last call.
    pub fn take_dropped_batches(&self) -> usize {
        self.dropped_batches.swap(0, Ordering::Relaxed)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::telemetry::events::RawEvent;

    fn poll_end() -> RawEvent {
        RawEvent::PollEnd {
            timestamp_nanos: 1000,
            worker_id: crate::telemetry::format::WorkerId::from(0usize),
        }
    }

    #[test]
    fn test_collector_creation() {
        let collector = CentralCollector::new();
        assert_eq!(collector.drain().len(), 0);
    }

    #[test]
    fn test_accept_flush() {
        let collector = CentralCollector::new();
        collector.accept_flush(vec![poll_end()]);
        let drained = collector.drain();
        assert_eq!(drained.len(), 1);
        assert_eq!(drained[0].len(), 1);
    }

    #[test]
    fn test_drain_clears_buffers() {
        let collector = CentralCollector::new();
        collector.accept_flush(vec![poll_end()]);
        assert_eq!(collector.drain().len(), 1);
        assert_eq!(collector.drain().len(), 0);
    }

    #[test]
    fn test_bounded_evicts_oldest_when_full() {
        let collector = CentralCollector::with_capacity(2);
        collector.accept_flush(vec![poll_end()]); // oldest — will be evicted
        collector.accept_flush(vec![poll_end(), poll_end()]);
        collector.accept_flush(vec![poll_end(), poll_end(), poll_end()]); // evicts first
        assert_eq!(collector.take_dropped_batches(), 1);
        let drained = collector.drain();
        assert_eq!(drained.len(), 2);
        // oldest (len=1) was evicted; remaining are len=2 and len=3
        assert_eq!(drained[0].len(), 2);
        assert_eq!(drained[1].len(), 3);
    }
}
