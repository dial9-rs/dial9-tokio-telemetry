use crate::telemetry::events::TelemetryEvent;
use std::sync::Mutex;

pub struct CentralCollector {
    buffers: Mutex<Vec<Vec<TelemetryEvent>>>,
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

    pub fn accept_flush(&self, buffer: Vec<TelemetryEvent>) {
        self.buffers.lock().unwrap().push(buffer);
    }

    pub fn drain(&self) -> Vec<Vec<TelemetryEvent>> {
        std::mem::take(&mut *self.buffers.lock().unwrap())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::telemetry::events::{EventType, MetricsSnapshot};

    #[test]
    fn test_collector_creation() {
        let collector = CentralCollector::new();
        let drained = collector.drain();
        assert_eq!(drained.len(), 0);
    }

    #[test]
    fn test_accept_flush() {
        let collector = CentralCollector::new();
        let metrics = MetricsSnapshot {
            timestamp_nanos: 1000,
            worker_id: 0,
            global_queue_depth: 5,
            worker_local_queue_depth: 2,
            cpu_time_nanos: 0,
            sched_wait_delta_nanos: 0,
        };
        let event = TelemetryEvent::new(EventType::PollStart, metrics);
        let buffer = vec![event];

        collector.accept_flush(buffer);
        let drained = collector.drain();
        assert_eq!(drained.len(), 1);
        assert_eq!(drained[0].len(), 1);
    }

    #[test]
    fn test_multiple_flushes() {
        let collector = CentralCollector::new();
        let metrics = MetricsSnapshot {
            timestamp_nanos: 1000,
            worker_id: 0,
            global_queue_depth: 5,
            worker_local_queue_depth: 2,
            cpu_time_nanos: 0,
            sched_wait_delta_nanos: 0,
        };

        for i in 0..5 {
            let event = TelemetryEvent::new(EventType::PollStart, metrics);
            let buffer = vec![event; i + 1];
            collector.accept_flush(buffer);
        }

        let drained = collector.drain();
        assert_eq!(drained.len(), 5);
        assert_eq!(drained[0].len(), 1);
        assert_eq!(drained[4].len(), 5);
    }

    #[test]
    fn test_drain_clears_buffers() {
        let collector = CentralCollector::new();
        let metrics = MetricsSnapshot {
            timestamp_nanos: 1000,
            worker_id: 0,
            global_queue_depth: 5,
            worker_local_queue_depth: 2,
            cpu_time_nanos: 0,
            sched_wait_delta_nanos: 0,
        };
        let event = TelemetryEvent::new(EventType::PollStart, metrics);

        collector.accept_flush(vec![event]);
        let drained1 = collector.drain();
        assert_eq!(drained1.len(), 1);

        let drained2 = collector.drain();
        assert_eq!(drained2.len(), 0);
    }
}
