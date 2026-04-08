#[cfg(feature = "cpu-profiling")]
use super::shared_state::SharedState;
use crate::telemetry::collector::Batch;
#[cfg(feature = "cpu-profiling")]
use crate::telemetry::events::{CpuSampleData, ThreadRole};
#[cfg(feature = "cpu-profiling")]
use crate::telemetry::format::WorkerId;
use crate::telemetry::writer::TraceWriter;

/// Intermediate layer between the recorder and the raw `TraceWriter`.
///
/// Owns the writer and the CPU profiler. Its API is roughly:
///
/// - `write_raw_event(raw)` — encode and write a single event (test only)
/// - `write_cpu_event(event)` — write a CPU sample event
/// - `flush_cpu(shared)` — drain CPU/sched profilers into the trace via `write_cpu_event`
/// - `flush()` — flush the underlying writer
pub(crate) struct EventWriter {
    pub(super) writer: Box<dyn TraceWriter>,
    events_written: u64,
    #[cfg(feature = "cpu-profiling")]
    pub(super) cpu_profiler: Option<crate::telemetry::cpu_profile::CpuProfiler>,
}

impl EventWriter {
    pub(crate) fn new(writer: Box<dyn TraceWriter>) -> Self {
        Self {
            writer,
            events_written: 0,
            #[cfg(feature = "cpu-profiling")]
            cpu_profiler: None,
        }
    }

    pub(crate) fn events_written(&self) -> u64 {
        self.events_written
    }

    /// Encode a RawEvent into a batch and write it through the writer.
    #[cfg(test)]
    pub(crate) fn write_raw_event(
        &mut self,
        raw: crate::telemetry::events::RawEvent,
    ) -> std::io::Result<()> {
        use crate::telemetry::buffer::ThreadLocalBuffer;
        let encoded_bytes = ThreadLocalBuffer::encode_single(&raw);
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

    /// Drain CPU/sched profilers and write their events into the trace.
    #[cfg(feature = "cpu-profiling")]
    pub(crate) fn flush_cpu(&mut self, shared: &SharedState) {
        // Snapshot thread_roles once per flush cycle.
        let roles = shared.thread_roles.lock().unwrap().clone();

        let resolve = |tid: u32| -> WorkerId {
            match roles.get(&tid) {
                Some(ThreadRole::Worker(id)) => WorkerId::from(*id),
                Some(ThreadRole::Blocking) => WorkerId::BLOCKING,
                None => WorkerId::UNKNOWN,
            }
        };

        if let Some(mut profiler) = self.cpu_profiler.take() {
            profiler.drain(|raw, thread_name| {
                use crate::telemetry::{buffer::record_event, events::RawEvent};

                let worker_id = resolve(raw.tid);
                let data = CpuSampleData {
                    timestamp_nanos: raw.timestamp_nanos,
                    worker_id,
                    tid: raw.tid,
                    source: raw.source,
                    callchain: raw.callchain,
                    thread_name: thread_name.cloned(),
                };
                record_event(RawEvent::CpuSample(Box::new(data)), &shared.collector);
            });
            self.cpu_profiler = Some(profiler);
        }

        {
            let mut shared_profiler = shared.sched_profiler.lock().unwrap();
            if let Some(ref mut profiler) = *shared_profiler {
                profiler.drain(|raw| {
                    use crate::telemetry::{buffer::record_event, events::RawEvent};

                    let data = CpuSampleData {
                        timestamp_nanos: raw.timestamp_nanos,
                        worker_id: resolve(raw.tid),
                        tid: raw.tid,
                        source: raw.source,
                        callchain: raw.callchain,
                        // TODO: we should be able to also track thread name here.
                        // sampler is running on worker threads so no thread name
                        thread_name: None,
                    };
                    record_event(RawEvent::CpuSample(Box::new(data)), &shared.collector);
                });
            }
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

    pub(crate) fn finalize(&mut self) -> std::io::Result<()> {
        self.writer.finalize()
    }
}
