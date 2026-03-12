use super::flush_state::FlushState;
#[cfg(feature = "cpu-profiling")]
use super::shared_state::SharedState;
use crate::telemetry::events::RawEvent;
#[cfg(feature = "cpu-profiling")]
use crate::telemetry::events::{BLOCKING_WORKER, TelemetryEvent, ThreadRole, UNKNOWN_WORKER};
use crate::telemetry::writer::{TraceWriter, WriteAtomicResult};

#[cfg(feature = "cpu-profiling")]
use super::cpu_flush_state::CpuFlushState;

/// Intermediate layer between the recorder and the raw `TraceWriter`.
///
/// Owns the writer, spawn-location interning (`FlushState`), CPU-profiling
/// interning state (`CpuFlushState`), and the CPU profiler itself. Its API is
/// roughly:
///
/// - `write_raw_event(raw)` — intern locations, handle rotation, write atomically
/// - `write_cpu_event(event)` — intern CPU defs, handle rotation, write atomically
/// - `flush_cpu(shared)` — drain CPU/sched profilers into the trace via `write_cpu_event`
/// - `flush()` — flush the underlying writer
pub(crate) struct EventWriter {
    pub(super) writer: Box<dyn TraceWriter>,
    pub(super) flush_state: FlushState,
    #[cfg(feature = "cpu-profiling")]
    pub(super) cpu_flush: Option<CpuFlushState>,
    #[cfg(feature = "cpu-profiling")]
    pub(super) cpu_profiler: Option<crate::telemetry::cpu_profile::CpuProfiler>,
}

impl EventWriter {
    pub(crate) fn new(writer: Box<dyn TraceWriter>) -> Self {
        Self {
            writer,
            flush_state: FlushState::new(),
            #[cfg(feature = "cpu-profiling")]
            cpu_flush: None,
            #[cfg(feature = "cpu-profiling")]
            cpu_profiler: None,
        }
    }

    fn handle_rotation(&mut self) {
        self.flush_state.on_rotate();
        #[cfg(feature = "cpu-profiling")]
        if let Some(ref mut cpu) = self.cpu_flush {
            cpu.on_rotate();
        }
    }

    /// Convert a RawEvent to wire format, interning locations as needed.
    pub(crate) fn write_raw_event(&mut self, raw: RawEvent) -> std::io::Result<WriteAtomicResult> {
        if self.writer.take_rotated() {
            self.handle_rotation();
        }
        let events = self.flush_state.resolve(raw);
        match self.writer.write_atomic(&events)? {
            WriteAtomicResult::Written => Ok(WriteAtomicResult::Written),
            WriteAtomicResult::OversizedBatch => Ok(WriteAtomicResult::OversizedBatch),
            WriteAtomicResult::Rotated => {
                let was_rotated = self.writer.take_rotated();
                debug_assert!(
                    was_rotated,
                    "write atomic returned Rotated, but take_rotated() was false"
                );
                self.handle_rotation();
                let events = self.flush_state.resolve(raw);
                match self.writer.write_atomic(&events)? {
                    r @ (WriteAtomicResult::Written | WriteAtomicResult::OversizedBatch) => Ok(r),
                    WriteAtomicResult::Rotated => {
                        tracing::error!(
                            "double failed to write events after rotation — this is a bug, disabling"
                        );
                        Ok(WriteAtomicResult::Rotated)
                    }
                }
            }
        }
    }

    /// Write a single CPU event, emitting any necessary defs first and handling
    /// file rotation.
    ///
    /// This is the single entry point for all CPU event writing — both the
    /// profiler drain path ([`flush_cpu`](Self::flush_cpu)) and direct callers
    /// go through here.
    #[cfg(feature = "cpu-profiling")]
    pub(crate) fn write_cpu_event(&mut self, event: &TelemetryEvent) {
        if let Some(mut cpu) = self.cpu_flush.take() {
            let batch = cpu.collect_cpu_event_batch(event);
            match self.writer.write_atomic(&batch) {
                Ok(WriteAtomicResult::Rotated) => {
                    debug_assert!(
                        self.writer.take_rotated(),
                        "write_atomic returned Rotated but take_rotated is false"
                    );
                    self.flush_state.on_rotate();
                    cpu.on_rotate();
                    let batch = cpu.collect_cpu_event_batch(event);
                    let _ = self.writer.write_atomic(&batch);
                }
                Ok(WriteAtomicResult::Written | WriteAtomicResult::OversizedBatch) => {}
                Err(_) => {}
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
                let event = TelemetryEvent::CpuSample {
                    timestamp_nanos: raw.timestamp_nanos,
                    worker_id,
                    tid: raw.tid,
                    source: raw.source,
                    callchain: raw.callchain,
                };
                self.write_cpu_event(&event);
            });
            self.cpu_profiler = Some(profiler);
        }

        {
            let mut shared_profiler = shared.sched_profiler.lock().unwrap();
            if let Some(ref mut profiler) = *shared_profiler {
                profiler.drain(|raw| {
                    let event = TelemetryEvent::CpuSample {
                        timestamp_nanos: raw.timestamp_nanos,
                        worker_id: resolve(raw.tid),
                        tid: raw.tid,
                        source: raw.source,
                        callchain: raw.callchain,
                    };
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
