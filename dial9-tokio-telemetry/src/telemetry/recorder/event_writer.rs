#[cfg(feature = "cpu-profiling")]
use super::shared_state::SharedState;
use crate::telemetry::events::RawEvent;
#[cfg(feature = "cpu-profiling")]
use crate::telemetry::events::{BLOCKING_WORKER, CpuSampleData, ThreadRole, UNKNOWN_WORKER};
use crate::telemetry::writer::TraceWriter;

#[cfg(feature = "cpu-profiling")]
use super::cpu_flush_state::CpuFlushState;

/// Intermediate layer between the recorder and the raw `TraceWriter`.
///
/// Owns the writer, CPU-profiling interning state (`CpuFlushState`), and the
/// CPU profiler itself. Its API is roughly:
///
/// - `write_raw_event(raw)` — write event through the writer
/// - `write_cpu_event(event)` — intern CPU defs, write atomically
/// - `flush_cpu(shared)` — drain CPU/sched profilers into the trace via `write_cpu_event`
/// - `flush()` — flush the underlying writer
pub(crate) struct EventWriter {
    pub(super) writer: Box<dyn TraceWriter>,
    #[cfg(feature = "cpu-profiling")]
    pub(super) cpu_flush: Option<CpuFlushState>,
    #[cfg(feature = "cpu-profiling")]
    pub(super) cpu_profiler: Option<crate::telemetry::cpu_profile::CpuProfiler>,
}

impl EventWriter {
    pub(crate) fn new(writer: Box<dyn TraceWriter>) -> Self {
        Self {
            writer,
            #[cfg(feature = "cpu-profiling")]
            cpu_flush: None,
            #[cfg(feature = "cpu-profiling")]
            cpu_profiler: None,
        }
    }

    /// Write a RawEvent through the writer.
    pub(crate) fn write_raw_event(&mut self, raw: RawEvent) -> std::io::Result<()> {
        // Check if a previous write caused rotation so CPU state can be reset.
        #[cfg(feature = "cpu-profiling")]
        if self.writer.take_rotated() {
            if let Some(ref mut cpu) = self.cpu_flush {
                cpu.on_rotate();
            }
        }
        self.writer.write_atomic(&[raw])?;
        #[cfg(feature = "cpu-profiling")]
        if self.writer.take_rotated() {
            if let Some(ref mut cpu) = self.cpu_flush {
                cpu.on_rotate();
            }
        }
        Ok(())
    }

    /// Write a single CPU event, emitting any necessary defs first.
    #[cfg(feature = "cpu-profiling")]
    pub(crate) fn write_cpu_event(&mut self, event: &RawEvent) {
        if let Some(mut cpu) = self.cpu_flush.take() {
            let batch = cpu.collect_cpu_event_batch(event);
            if let Err(e) = self.writer.write_atomic(&batch) {
                tracing::warn!("failed to write CPU trace event: {e}");
            }
            if self.writer.take_rotated() {
                cpu.on_rotate();
            }
            self.cpu_flush = Some(cpu);
        }
    }

    /// Drain CPU/sched profilers and write their events into the trace.
    #[cfg(feature = "cpu-profiling")]
    pub(crate) fn flush_cpu(&mut self, shared: &SharedState) {
        // Snapshot thread_roles once per flush cycle.
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
                let worker_id = resolve(raw.tid);
                if let Some(ref mut cpu) = self.cpu_flush
                    && !cpu.thread_name_intern.contains_key(&raw.tid)
                    && let Some(name) = thread_name
                {
                    cpu.thread_name_intern.insert(raw.tid, name.to_string());
                }
                let event = RawEvent::CpuSample(Box::new(CpuSampleData {
                    timestamp_nanos: raw.timestamp_nanos,
                    worker_id,
                    tid: raw.tid,
                    source: raw.source,
                    callchain: raw.callchain,
                }));
                self.write_cpu_event(&event);
            });
            self.cpu_profiler = Some(profiler);
        }

        {
            let mut shared_profiler = shared.sched_profiler.lock().unwrap();
            if let Some(ref mut profiler) = *shared_profiler {
                profiler.drain(|raw| {
                    let event = RawEvent::CpuSample(Box::new(CpuSampleData {
                        timestamp_nanos: raw.timestamp_nanos,
                        worker_id: resolve(raw.tid),
                        tid: raw.tid,
                        source: raw.source,
                        callchain: raw.callchain,
                    }));
                    self.write_cpu_event(&event);
                });
            }
        }
    }

    pub(crate) fn flush(&mut self) -> std::io::Result<()> {
        self.writer.flush()
    }

    pub(crate) fn seal(&mut self) -> std::io::Result<()> {
        self.writer.seal()
    }
}
