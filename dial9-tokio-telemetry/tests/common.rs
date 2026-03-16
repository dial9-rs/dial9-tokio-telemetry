use dial9_tokio_telemetry::telemetry::events::{RawEvent, TelemetryEvent};
use dial9_tokio_telemetry::telemetry::writer::{EventResolver, TraceWriter};
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
pub struct CapturingWriter {
    events: Arc<Mutex<Vec<TelemetryEvent>>>,
    resolver: EventResolver,
}

impl CapturingWriter {
    /// Create a new writer and return a handle to the shared event buffer.
    pub fn new() -> (Self, Arc<Mutex<Vec<TelemetryEvent>>>) {
        let events = Arc::new(Mutex::new(Vec::new()));
        (
            Self {
                events: events.clone(),
                resolver: EventResolver::new(),
            },
            events,
        )
    }
}

impl TraceWriter for CapturingWriter {
    fn write_event(&mut self, event: &RawEvent) -> std::io::Result<()> {
        let resolved = self.resolver.resolve(event);
        self.events.lock().unwrap().extend(resolved);
        Ok(())
    }

    fn flush(&mut self) -> std::io::Result<()> {
        Ok(())
    }
}
