use crate::telemetry::events::RawEvent;
use std::sync::Mutex;

pub struct CentralCollector {
    buffers: Mutex<Vec<Vec<RawEvent>>>,
}

impl Default for CentralCollector {
    fn default() -> Self {
        Self::new()
    }
}

impl CentralCollector {
    pub fn new() -> Self {
        Self {
            buffers: Mutex::new(Vec::new()),
        }
    }

    pub fn accept_flush(&self, buffer: Vec<RawEvent>) {
        self.buffers.lock().unwrap().push(buffer);
    }

    pub fn drain(&self) -> Vec<Vec<RawEvent>> {
        std::mem::take(&mut *self.buffers.lock().unwrap())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::telemetry::events::RawEvent;

    fn poll_end() -> RawEvent {
        RawEvent::PollEnd {
            timestamp_nanos: 1000,
            worker_id: 0,
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
}
