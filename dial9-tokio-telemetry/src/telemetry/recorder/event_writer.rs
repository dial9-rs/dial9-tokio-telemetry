#[cfg(feature = "cpu-profiling")]
use super::shared_state::SharedState;
use crate::telemetry::events::RawEvent;
#[cfg(feature = "cpu-profiling")]
use crate::telemetry::events::{CpuSampleData, ThreadRole};
#[cfg(feature = "cpu-profiling")]
use crate::telemetry::format::WorkerId;
use crate::telemetry::writer::TraceWriter;

/// Intermediate layer between the recorder and the raw `TraceWriter`.
///
/// Owns the writer and the CPU profiler. Its API is roughly:
///
/// - `write_raw_event(raw)` — write event through the writer
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

    /// Write a RawEvent through the writer.
    pub(crate) fn write_raw_event(&mut self, raw: RawEvent) -> std::io::Result<()> {
        self.writer.write_event(&raw)?;
        self.events_written += 1;
        Ok(())
    }

    /// Write a single CPU sample event.
    #[cfg(feature = "cpu-profiling")]
    pub(crate) fn write_cpu_event(&mut self, data: &CpuSampleData) {
        let event = RawEvent::CpuSample(Box::new(data.clone()));
        if let Err(e) = self.writer.write_event(&event) {
            tracing::warn!("failed to write CPU trace event: {e}");
        }
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
                let worker_id = resolve(raw.tid);
                let data = CpuSampleData {
                    timestamp_nanos: raw.timestamp_nanos,
                    worker_id,
                    tid: raw.tid,
                    source: raw.source,
                    callchain: raw.callchain,
                    thread_name: thread_name.cloned(),
                };
                self.write_cpu_event(&data);
            });
            self.cpu_profiler = Some(profiler);
        }

        {
            let mut shared_profiler = shared.sched_profiler.lock().unwrap();
            if let Some(ref mut profiler) = *shared_profiler {
                profiler.drain(|raw| {
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
                    self.write_cpu_event(&data);
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

    pub(crate) fn flush(&mut self) -> std::io::Result<()> {
        self.writer.flush()
    }

    pub(crate) fn finalize(&mut self) -> std::io::Result<()> {
        self.writer.finalize()
    }
}
