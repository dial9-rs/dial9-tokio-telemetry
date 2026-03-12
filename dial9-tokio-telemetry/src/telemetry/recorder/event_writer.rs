#[cfg(feature = "cpu-profiling")]
use super::shared_state::SharedState;
use crate::telemetry::events::RawEvent;
#[cfg(feature = "cpu-profiling")]
use crate::telemetry::events::{BLOCKING_WORKER, ThreadRole, UNKNOWN_WORKER};
use crate::telemetry::writer::TraceWriter;

/// Intermediate layer between the recorder and the raw `TraceWriter`.
///
/// Owns the writer and CPU profiler. The writer handles all interning
/// (spawn locations, thread names, callframe symbols) internally.
///
/// - `write_raw_event(raw)` — delegates to writer
/// - `flush_cpu(shared)` — drain CPU/sched profilers, call writer.write_cpu_sample()
/// - `flush()` — flush the underlying writer
pub(crate) struct EventWriter {
    pub(super) writer: Box<dyn TraceWriter>,
    #[cfg(feature = "cpu-profiling")]
    pub(super) cpu_profiler: Option<crate::telemetry::cpu_profile::CpuProfiler>,
}

impl EventWriter {
    pub(crate) fn new(writer: Box<dyn TraceWriter>) -> Self {
        Self {
            writer,
            #[cfg(feature = "cpu-profiling")]
            cpu_profiler: None,
        }
    }

    /// Write a raw event. The writer handles interning and encoding.
    pub(crate) fn write_raw_event(&mut self, raw: RawEvent) -> std::io::Result<()> {
        self.writer.write_raw_event(raw)
    }

    /// Drain CPU/sched profilers and write their events into the trace.
    #[cfg(feature = "cpu-profiling")]
    pub(crate) fn flush_cpu(&mut self, shared: &SharedState) {
        let roles = shared.thread_roles.lock().unwrap().clone();

        let resolve = |tid: u32| -> usize {
            match roles.get(&tid) {
                Some(ThreadRole::Worker(id)) => *id,
                Some(ThreadRole::Blocking) => BLOCKING_WORKER,
                None => UNKNOWN_WORKER,
            }
        };

        if let Some(mut profiler) = self.cpu_profiler.take() {
            profiler.drain(|raw, thread_name| {
                let _ = self.writer.write_cpu_sample(
                    raw.timestamp_nanos,
                    resolve(raw.tid),
                    raw.tid,
                    raw.source,
                    &raw.callchain,
                    thread_name,
                );
            });
            self.cpu_profiler = Some(profiler);
        }

        {
            let mut shared_profiler = shared.sched_profiler.lock().unwrap();
            if let Some(ref mut profiler) = *shared_profiler {
                profiler.drain(|raw| {
                    let _ = self.writer.write_cpu_sample(
                        raw.timestamp_nanos,
                        resolve(raw.tid),
                        raw.tid,
                        raw.source,
                        &raw.callchain,
                        None,
                    );
                });
            }
        }
    }

    pub(crate) fn flush(&mut self) -> std::io::Result<()> {
        self.writer.flush()
    }
}
