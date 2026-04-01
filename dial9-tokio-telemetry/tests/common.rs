use dial9_tokio_telemetry::telemetry::Batch;
use dial9_tokio_telemetry::telemetry::TelemetryEvent;
use dial9_tokio_telemetry::telemetry::format::decode_events;
use dial9_tokio_telemetry::telemetry::writer::TraceWriter;
use std::sync::{Arc, Mutex};

/// A [`TraceWriter`] that accumulates all events into a shared `Vec`.
///
/// Encoded batches are decoded back into `RawEvent` variants so that
/// tests can inspect them uniformly regardless of the encoding path.
pub struct CapturingWriter {
    events: Arc<Mutex<Vec<TelemetryEvent>>>,
}

impl CapturingWriter {
    pub fn new() -> (Self, Arc<Mutex<Vec<TelemetryEvent>>>) {
        let events = Arc::new(Mutex::new(Vec::new()));
        (
            Self {
                events: events.clone(),
            },
            events,
        )
    }
}

impl TraceWriter for CapturingWriter {
    fn write_encoded_batch(&mut self, batch: &Batch) -> std::io::Result<()> {
        let events = decode_events(batch.encoded_bytes()).expect("invalid batch");
        self.events.lock().unwrap().extend_from_slice(&events);
        Ok(())
    }

    fn flush(&mut self) -> std::io::Result<()> {
        Ok(())
    }
}
