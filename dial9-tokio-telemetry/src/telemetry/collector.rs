use crossbeam_queue::ArrayQueue;
use std::sync::atomic::{AtomicUsize, Ordering};

/// Maximum number of batches (each up to 1024 events) that can be buffered.
/// Beyond this, the oldest batch is evicted — the queue acts as a ring buffer
/// so the most recent data is always preserved.
const DEFAULT_CAPACITY: usize = 1024;

pub(crate) struct Batch {
    pub encoded_bytes: Vec<u8>,
    pub event_count: u64,
}

pub(crate) struct CentralCollector {
    queue: ArrayQueue<Batch>,
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

    pub fn accept_flush(&self, batch: Batch) {
        if let Some(_evicted) = self.queue.force_push(batch) {
            self.dropped_batches.fetch_add(1, Ordering::Relaxed);
        }
    }

    pub fn next(&self) -> Option<Batch> {
        self.queue.pop()
    }

    /// Returns the number of batches dropped since the last call.
    pub fn take_dropped_batches(&self) -> usize {
        self.dropped_batches.swap(0, Ordering::Relaxed)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn dummy_batch(size: usize) -> Batch {
        Batch {
            encoded_bytes: vec![0u8; size],
            event_count: 1,
        }
    }

    #[test]
    fn test_drain_clears_buffers() {
        let collector = CentralCollector::new();
        collector.accept_flush(dummy_batch(10));
        assert!(collector.next().is_some());
        assert!(collector.next().is_none());
    }

    fn drain(collector: &CentralCollector) -> Vec<Vec<u8>> {
        let mut out = vec![];
        while let Some(batch) = collector.next() {
            out.push(batch.encoded_bytes);
        }
        out
    }

    #[test]
    fn test_bounded_evicts_oldest_when_full() {
        let collector = CentralCollector::with_capacity(2);
        collector.accept_flush(dummy_batch(1)); // oldest — will be evicted
        collector.accept_flush(dummy_batch(2));
        collector.accept_flush(dummy_batch(3)); // evicts first
        assert_eq!(collector.take_dropped_batches(), 1);
        let drained = drain(&collector);
        assert_eq!(drained.len(), 2);
        // oldest (len=1) was evicted; remaining are len=2 and len=3
        assert_eq!(drained[0].len(), 2);
        assert_eq!(drained[1].len(), 3);
    }
}
