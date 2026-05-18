use super::shared_state::SharedState;
use crate::telemetry::collector::Batch;
use crate::telemetry::writer::TraceWriter;

/// Intermediate layer between the recorder and the raw `TraceWriter`.
///
/// Owns the writer. Its API is roughly:
///
/// - `write_raw_event(event)` — encode and write a single event (test only)
/// - `flush_sources(shared)` — drain data sources into the trace
/// - `flush()` — flush the underlying writer
pub(crate) struct EventWriter {
    pub(super) writer: Box<dyn TraceWriter>,
    events_written: u64,
}

impl EventWriter {
    pub(crate) fn new(writer: Box<dyn TraceWriter>) -> Self {
        Self {
            writer,
            events_written: 0,
        }
    }

    pub(crate) fn events_written(&self) -> u64 {
        self.events_written
    }

    // TODO: delete/refactor this method, it is only used in tests.
    /// Encode a single event into a batch and write it through the writer.
    #[cfg(all(test, feature = "cpu-profiling"))]
    pub(crate) fn write_raw_event(
        &mut self,
        event: &dyn crate::telemetry::buffer::Encodable,
    ) -> std::io::Result<()> {
        use crate::telemetry::buffer::ThreadLocalBuffer;
        let encoded_bytes = ThreadLocalBuffer::encode_single(event);
        let batch = Batch {
            encoded_bytes,
            event_count: 1,
        };
        self.writer.write_encoded_batch(&batch)?;
        self.events_written += 1;
        Ok(())
    }

    /// Transcode an entire batch through the writer, correctly accounting for
    /// the number of events the batch contains.
    pub(crate) fn write_encoded_batch(&mut self, batch: &Batch) -> std::io::Result<()> {
        self.writer.write_encoded_batch(batch)?;
        self.events_written += batch.event_count;
        Ok(())
    }

    /// Drain data sources and write their events into the trace.
    pub(crate) fn flush_sources(&mut self, shared: &SharedState) {
        use super::source::FlushContext;

        let roles = shared.thread_roles.lock().unwrap().clone();

        let ctx = FlushContext {
            collector: &shared.collector,
            drain_epoch: &shared.drain_epoch,
            thread_roles: &roles,
        };

        let mut sources = shared.sources.lock().unwrap();
        for source in sources.iter_mut() {
            source.flush(&ctx);
        }
    }

    pub(crate) fn segment_metadata(&self) -> &[(String, String)] {
        self.writer.segment_metadata()
    }

    pub(crate) fn update_segment_metadata(&mut self, entries: Vec<(String, String)>) {
        self.writer.update_segment_metadata(entries);
    }

    pub(crate) fn write_current_segment_metadata(&mut self) -> std::io::Result<()> {
        self.writer.write_current_segment_metadata()
    }

    pub(crate) fn flush(&mut self) -> std::io::Result<()> {
        self.writer.flush()
    }

    pub(crate) fn should_drain(&self) -> bool {
        self.writer.should_drain()
    }

    pub(crate) fn drained(&mut self) -> std::io::Result<bool> {
        self.writer.drained()
    }

    pub(crate) fn finalize(&mut self) -> std::io::Result<()> {
        self.writer.finalize()
    }
}
