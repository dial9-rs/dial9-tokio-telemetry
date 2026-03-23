use dial9_tokio_telemetry::telemetry::events::RawEvent;
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
pub struct CapturingWriter {
    events: Arc<Mutex<Vec<RawEvent>>>,
    encoded_batches: Arc<Mutex<Vec<Vec<u8>>>>,
}

impl CapturingWriter {
    /// Create a new writer and return a handle to the shared event buffer.
    pub fn new() -> (Self, Arc<Mutex<Vec<RawEvent>>>) {
        let events = Arc::new(Mutex::new(Vec::new()));
        (
            Self {
                events: events.clone(),
                encoded_batches: Arc::new(Mutex::new(Vec::new())),
            },
            events,
        )
    }
}

impl TraceWriter for CapturingWriter {
    fn write_event(&mut self, event: &RawEvent) -> std::io::Result<()> {
        self.events.lock().unwrap().push(event.clone());
        Ok(())
    }

    fn write_encoded_batch(&mut self, bytes: &[u8]) -> std::io::Result<()> {
        self.encoded_batches.lock().unwrap().push(bytes.to_vec());
        Ok(())
    }

    fn flush(&mut self) -> std::io::Result<()> {
        Ok(())
    }
}
