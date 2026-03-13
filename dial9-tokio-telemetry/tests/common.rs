use dial9_tokio_telemetry::telemetry::events::TelemetryEvent;
use dial9_tokio_telemetry::telemetry::writer::TraceWriter;
use std::sync::{Arc, Mutex};

/// A [`TraceWriter`] that accumulates all events into a shared `Vec`.
///
/// Construct with [`CapturingWriter::new`] and pass the returned
/// `Arc<Mutex<Vec<TelemetryEvent>>>` wherever you need to inspect the
/// captured events after the runtime has been dropped.
///
/// ```rust,ignore
/// let (writer, events) = CapturingWriter::new();
/// // ... build runtime with writer ...
/// let captured = events.lock().unwrap();
/// ```
pub struct CapturingWriter(Arc<Mutex<Vec<TelemetryEvent>>>);

impl CapturingWriter {
    /// Create a new writer and return a handle to the shared event buffer.
    pub fn new() -> (Self, Arc<Mutex<Vec<TelemetryEvent>>>) {
        let events = Arc::new(Mutex::new(Vec::new()));
        (Self(events.clone()), events)
    }
}

impl TraceWriter for CapturingWriter {
    fn write_event(&mut self, event: &TelemetryEvent) -> std::io::Result<()> {
        self.0.lock().unwrap().push(event.clone());
        Ok(())
    }

    fn write_batch(&mut self, events: &[TelemetryEvent]) -> std::io::Result<()> {
        self.0.lock().unwrap().extend_from_slice(events);
        Ok(())
    }

    fn flush(&mut self) -> std::io::Result<()> {
        Ok(())
    }
}
