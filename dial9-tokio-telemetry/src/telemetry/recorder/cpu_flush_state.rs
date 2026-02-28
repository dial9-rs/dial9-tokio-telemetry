#[cfg(feature = "cpu-profiling")]
use crate::telemetry::events::{TelemetryEvent, UNKNOWN_WORKER};
#[cfg(feature = "cpu-profiling")]
use std::collections::{HashMap, HashSet};

/// CPU-profiling interning state: tracks which defs (thread names, callframe
/// symbols) have been emitted in the current file so they can be re-emitted
/// after rotation.
///
/// This struct is intentionally *not* aware of writers, profilers, or flush
/// orchestration — that lives in [`super::event_writer::EventWriter`].
#[cfg(feature = "cpu-profiling")]
pub(super) struct CpuFlushState {
    /// When true, symbolicate callframe addresses and emit CallframeDef events.
    pub(super) inline_callframe_symbols: bool,
    /// Addresses already symbolicated (across all files). Maps addr → (symbol, location).
    pub(super) callframe_intern: HashMap<u64, (String, Option<String>)>,
    /// Addresses whose CallframeDef has been emitted in the current file.
    callframe_emitted_this_file: HashSet<u64>,
    /// tid → thread name, cached across files. Only populated for non-worker tids.
    pub(super) thread_name_intern: HashMap<u32, String>,
    /// tids whose ThreadNameDef has been emitted in the current file.
    thread_name_emitted_this_file: HashSet<u32>,
}

#[cfg(feature = "cpu-profiling")]
impl CpuFlushState {
    pub(super) fn new() -> Self {
        Self {
            inline_callframe_symbols: false,
            callframe_intern: HashMap::new(),
            callframe_emitted_this_file: HashSet::new(),
            thread_name_intern: HashMap::new(),
            thread_name_emitted_this_file: HashSet::new(),
        }
    }

    /// Called on file rotation — clear per-file tracking sets.
    pub(super) fn on_rotate(&mut self) {
        self.callframe_emitted_this_file.clear();
        self.thread_name_emitted_this_file.clear();
    }

    /// Collect the prerequisite def events for a CPU event, updating per-file tracking sets.
    pub(super) fn collect_cpu_event_batch(
        &mut self,
        event: &TelemetryEvent,
    ) -> Vec<TelemetryEvent> {
        let mut batch = Vec::new();
        if let TelemetryEvent::CpuSample { worker_id, tid, .. } = event
            && *worker_id == UNKNOWN_WORKER
            && !self.thread_name_emitted_this_file.contains(tid)
        {
            if let Some(name) = self.thread_name_intern.get(tid) {
                batch.push(TelemetryEvent::ThreadNameDef {
                    tid: *tid,
                    name: name.clone(),
                });
            }
            self.thread_name_emitted_this_file.insert(*tid);
        }
        if self.inline_callframe_symbols
            && let TelemetryEvent::CpuSample { callchain, .. } = event
        {
            for &addr in callchain {
                if !self.callframe_emitted_this_file.contains(&addr) {
                    self.callframe_intern.entry(addr).or_insert_with(|| {
                        let sym = dial9_perf_self_profile::resolve_symbol(addr);
                        let symbol = sym.name.unwrap_or_else(|| format!("{:#x}", addr));
                        let location = sym.code_info.map(|info| match info.line {
                            Some(line) => format!("{}:{}", info.file, line),
                            None => info.file,
                        });
                        (symbol, location)
                    });
                    let (symbol, location) = self.callframe_intern[&addr].clone();
                    batch.push(TelemetryEvent::CallframeDef {
                        address: addr,
                        symbol,
                        location,
                    });
                    self.callframe_emitted_this_file.insert(addr);
                }
            }
        }
        batch.push(event.clone());
        batch
    }
}
