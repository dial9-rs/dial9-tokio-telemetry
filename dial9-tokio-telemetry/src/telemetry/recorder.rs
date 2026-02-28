use crate::telemetry::buffer::BUFFER;
use crate::telemetry::collector::CentralCollector;
use crate::telemetry::events::{RawEvent, SchedStat, TelemetryEvent, UNKNOWN_WORKER};
use crate::telemetry::task_metadata::{SpawnLocationId, TaskId};
use crate::telemetry::writer::{TraceWriter, WriteAtomicResult};
use arc_swap::ArcSwap;
use smallvec::SmallVec;
use std::cell::Cell;
use std::collections::{HashMap, HashSet};
use std::panic::Location;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};
use tokio::runtime::{Handle, RuntimeMetrics};

thread_local! {
    /// Cached tokio worker index for this thread. `None` means not yet resolved.
    /// Once resolved, the worker ID is stable for the lifetime of the thread—a thread
    /// won't become a *different* worker, though it may stop being a worker entirely.
    static WORKER_ID: Cell<Option<usize>> = const { Cell::new(None) };
    /// Negative cache: true once we've confirmed this thread is NOT a worker.
    /// Only set after a full scan with metrics available.
    static NOT_A_WORKER: Cell<bool> = const { Cell::new(false) };
    /// Whether we've already registered this thread's tid→worker mapping.
    static TID_EMITTED: Cell<bool> = const { Cell::new(false) };
    /// schedstat wait_time_ns captured at park time, used to compute delta on unpark.
    static PARKED_SCHED_WAIT: Cell<u64> = const { Cell::new(0) };
}

/// Resolve the current thread's tokio worker index, caching in TLS.
/// Returns None if the thread is not a tokio worker.
///
/// The result is cached permanently in TLS because a thread's worker identity
/// is stable: it won't become a different worker, it can only stop being one.
///
/// On first resolution, eagerly registers the OS tid → worker mapping in
/// `SharedState.worker_tids` so the CPU profiler can attribute samples
/// immediately (without waiting for a flush cycle).
fn resolve_worker_id(
    metrics: &ArcSwap<Option<RuntimeMetrics>>,
    shared: Option<&SharedState>,
) -> Option<usize> {
    #[cfg(not(feature = "cpu-profiling"))]
    let _ = shared;
    WORKER_ID.with(|cell| {
        if let Some(id) = cell.get() {
            return Some(id);
        }
        if NOT_A_WORKER.with(|c| c.get()) {
            return None;
        }
        let tid = std::thread::current().id();
        if let Some(ref m) = **metrics.load() {
            for i in 0..m.num_workers() {
                if m.worker_thread_id(i) == Some(tid) {
                    cell.set(Some(i));
                    // Eagerly register tid→worker mapping for CPU profiling
                    #[cfg(feature = "cpu-profiling")]
                    if let Some(shared) = shared {
                        TID_EMITTED.with(|emitted| {
                            if !emitted.get() {
                                emitted.set(true);
                                let os_tid = crate::telemetry::events::current_tid();
                                shared.worker_tids.lock().unwrap().insert(os_tid, i);
                            }
                        });
                    }
                    return Some(i);
                }
            }
            // Metrics available but no match — this thread is definitely not a worker.
            NOT_A_WORKER.with(|c| c.set(true));
        }
        None
    })
}

/// Get the current worker ID as u8 (255 if unknown). Used by Traced waker.
pub(crate) fn current_worker_id(metrics: &ArcSwap<Option<RuntimeMetrics>>) -> u8 {
    match resolve_worker_id(metrics, None) {
        Some(id) if id <= 254 => id as u8,
        _ => 255,
    }
}

/// Shared state accessed lock-free by callbacks on the hot path.
/// No spawn location tracking here — all interning happens in the flush thread.
pub(crate) struct SharedState {
    pub(crate) enabled: AtomicBool,
    pub(crate) collector: CentralCollector,
    pub(crate) start_time: Instant,
    pub(crate) metrics: ArcSwap<Option<RuntimeMetrics>>,
    /// OS tid → worker_id mapping, populated eagerly by worker threads on first
    /// `resolve_worker_id`. Workers only register once, so contention is negligible.
    /// Read by the CPU profiler during flush to attribute samples to workers.
    #[cfg(feature = "cpu-profiling")]
    pub(crate) worker_tids: Mutex<HashMap<u32, usize>>,
    /// Per-thread sched event profiler, shared with worker callbacks for track/stop.
    /// Only populated when sched event capture is enabled.
    #[cfg(feature = "cpu-profiling")]
    pub(crate) sched_profiler: Mutex<Option<crate::telemetry::cpu_profile::SchedProfiler>>,
}

impl SharedState {
    fn new(start_time: Instant) -> Self {
        Self {
            enabled: AtomicBool::new(false),
            collector: CentralCollector::new(),
            start_time,
            metrics: ArcSwap::from_pointee(None),
            #[cfg(feature = "cpu-profiling")]
            worker_tids: Mutex::new(HashMap::new()),
            #[cfg(feature = "cpu-profiling")]
            sched_profiler: Mutex::new(None),
        }
    }

    pub(crate) fn timestamp_nanos(&self) -> u64 {
        self.start_time.elapsed().as_nanos() as u64
    }

    /// Build a [`RawEvent::WakeEvent`] for the task identified by `woken_task_id`.
    ///
    /// The waker task id (the task currently running `wake_by_ref`) and the
    /// target worker are resolved automatically from thread-local state so
    /// callers don't need to reach into `SharedState` fields directly.
    pub(crate) fn create_wake_event(&self, woken_task_id: TaskId) -> RawEvent {
        let waker_task_id = tokio::task::try_id()
            .map(TaskId::from)
            .unwrap_or(TaskId::from_u32(0));
        let target_worker = current_worker_id(&self.metrics);
        RawEvent::WakeEvent {
            timestamp_nanos: self.timestamp_nanos(),
            waker_task_id,
            woken_task_id,
            target_worker,
        }
    }

    /// Record a queue-depth sample directly into the collector, bypassing the
    /// thread-local buffer.  The enabled flag is checked; if telemetry is off
    /// the sample is silently dropped.
    pub(crate) fn record_queue_sample(&self, global_queue_depth: usize) {
        if !self.enabled.load(Ordering::Relaxed) {
            return;
        }
        self.collector.accept_flush(vec![RawEvent::QueueSample {
            timestamp_nanos: self.timestamp_nanos(),
            global_queue_depth,
        }]);
    }

    pub(crate) fn record_event(&self, event: RawEvent) {
        if !self.enabled.load(Ordering::Relaxed) {
            return;
        }
        BUFFER.with(|buf| {
            let mut buf = buf.borrow_mut();
            buf.record_event(event);
            // Determine event type for flush decision
            let should_flush = buf.should_flush() || matches!(event, RawEvent::WorkerPark { .. });
            if should_flush {
                self.collector.accept_flush(buf.flush());
            }
        });
    }

    fn make_poll_start(&self, location: &'static Location<'static>, task_id: TaskId) -> RawEvent {
        let worker_id = resolve_worker_id(&self.metrics, Some(self));
        let metrics_guard = self.metrics.load();
        let worker_local_queue_depth =
            if let (Some(worker_id), Some(metrics)) = (worker_id, &**metrics_guard) {
                metrics.worker_local_queue_depth(worker_id)
            } else {
                0
            };
        RawEvent::PollStart {
            timestamp_nanos: self.start_time.elapsed().as_nanos() as u64,
            worker_id: worker_id.unwrap_or(UNKNOWN_WORKER),
            worker_local_queue_depth,
            task_id,
            location,
        }
    }

    fn make_poll_end(&self) -> RawEvent {
        let worker_id = resolve_worker_id(&self.metrics, Some(self));
        RawEvent::PollEnd {
            timestamp_nanos: self.start_time.elapsed().as_nanos() as u64,
            worker_id: worker_id.unwrap_or(UNKNOWN_WORKER),
        }
    }

    fn make_worker_park(&self) -> RawEvent {
        let worker_id = resolve_worker_id(&self.metrics, Some(self));
        let metrics_guard = self.metrics.load();
        let worker_local_queue_depth =
            if let (Some(worker_id), Some(metrics)) = (worker_id, &**metrics_guard) {
                metrics.worker_local_queue_depth(worker_id)
            } else {
                0
            };
        let cpu_time_nanos = crate::telemetry::events::thread_cpu_time_nanos();
        if let Ok(ss) = SchedStat::read_current() {
            PARKED_SCHED_WAIT.with(|c| c.set(ss.wait_time_ns));
        }
        RawEvent::WorkerPark {
            timestamp_nanos: self.start_time.elapsed().as_nanos() as u64,
            worker_id: worker_id.unwrap_or(UNKNOWN_WORKER),
            worker_local_queue_depth,
            cpu_time_nanos,
        }
    }

    fn make_worker_unpark(&self) -> RawEvent {
        let worker_id = resolve_worker_id(&self.metrics, Some(self));
        let metrics_guard = self.metrics.load();
        let worker_local_queue_depth =
            if let (Some(worker_id), Some(metrics)) = (worker_id, &**metrics_guard) {
                metrics.worker_local_queue_depth(worker_id)
            } else {
                0
            };
        let cpu_time_nanos = crate::telemetry::events::thread_cpu_time_nanos();
        let sched_wait_delta_nanos = if let Ok(ss) = SchedStat::read_current() {
            let prev = PARKED_SCHED_WAIT.with(|c| c.get());
            ss.wait_time_ns.saturating_sub(prev)
        } else {
            0
        };
        RawEvent::WorkerUnpark {
            timestamp_nanos: self.start_time.elapsed().as_nanos() as u64,
            worker_id: worker_id.unwrap_or(UNKNOWN_WORKER),
            worker_local_queue_depth,
            cpu_time_nanos,
            sched_wait_delta_nanos,
        }
    }
}

/// Flush-thread state for interning spawn locations and tracking per-file emissions.
struct FlushState {
    /// Location pointer (as usize) → SpawnLocationId. Only touched by flush thread.
    intern_map: HashMap<usize, SpawnLocationId>,
    /// SpawnLocationId → location string.
    intern_strings: Vec<String>,
    /// Which SpawnLocationIds have been emitted as SpawnLocationDef in the current file.
    emitted_this_file: HashSet<SpawnLocationId>,
    next_id: u16,
}

impl FlushState {
    fn new() -> Self {
        let intern_strings = vec!["<unknown>".to_string()];
        Self {
            intern_map: HashMap::new(),
            intern_strings,
            emitted_this_file: HashSet::new(),
            next_id: 1,
        }
    }

    /// Intern a location, returning its SpawnLocationId.
    fn intern(&mut self, location: &'static Location<'static>) -> SpawnLocationId {
        let ptr = location as *const Location<'static> as usize;
        if let Some(&id) = self.intern_map.get(&ptr) {
            return id;
        }
        let id = SpawnLocationId(self.next_id);
        self.next_id += 1;
        self.intern_map.insert(ptr, id);
        self.intern_strings.push(format!(
            "{}:{}:{}",
            location.file(),
            location.line(),
            location.column()
        ));
        id
    }

    /// If this id hasn't been emitted in the current file, push its def into `defs`.
    fn collect_def(&mut self, id: SpawnLocationId, defs: &mut SmallVec<[TelemetryEvent; 3]>) {
        if self.emitted_this_file.insert(id) {
            let loc = self.intern_strings[id.as_u16() as usize].clone();
            defs.push(TelemetryEvent::SpawnLocationDef { id, location: loc });
        }
    }

    /// Resolve a RawEvent into a SmallVec of wire events: defs first, then the event itself.
    fn resolve(&mut self, raw: RawEvent) -> SmallVec<[TelemetryEvent; 3]> {
        let mut events = SmallVec::new();
        match raw {
            RawEvent::TaskSpawn { task_id, location } => {
                let spawn_loc_id = self.intern(location);
                self.collect_def(spawn_loc_id, &mut events);
                events.push(TelemetryEvent::TaskSpawn {
                    task_id,
                    spawn_loc_id,
                });
            }
            RawEvent::PollStart {
                timestamp_nanos,
                worker_id,
                worker_local_queue_depth,
                task_id,
                location,
            } => {
                let spawn_loc_id = self.intern(location);
                self.collect_def(spawn_loc_id, &mut events);
                events.push(TelemetryEvent::PollStart {
                    timestamp_nanos,
                    worker_id,
                    worker_local_queue_depth,
                    task_id,
                    spawn_loc_id,
                });
            }
            RawEvent::PollEnd {
                timestamp_nanos,
                worker_id,
            } => {
                events.push(TelemetryEvent::PollEnd {
                    timestamp_nanos,
                    worker_id,
                });
            }
            RawEvent::WorkerPark {
                timestamp_nanos,
                worker_id,
                worker_local_queue_depth,
                cpu_time_nanos,
            } => {
                events.push(TelemetryEvent::WorkerPark {
                    timestamp_nanos,
                    worker_id,
                    worker_local_queue_depth,
                    cpu_time_nanos,
                });
            }
            RawEvent::WorkerUnpark {
                timestamp_nanos,
                worker_id,
                worker_local_queue_depth,
                cpu_time_nanos,
                sched_wait_delta_nanos,
            } => {
                events.push(TelemetryEvent::WorkerUnpark {
                    timestamp_nanos,
                    worker_id,
                    worker_local_queue_depth,
                    cpu_time_nanos,
                    sched_wait_delta_nanos,
                });
            }
            RawEvent::QueueSample {
                timestamp_nanos,
                global_queue_depth,
            } => {
                events.push(TelemetryEvent::QueueSample {
                    timestamp_nanos,
                    global_queue_depth,
                });
            }
            RawEvent::WakeEvent {
                timestamp_nanos,
                waker_task_id,
                woken_task_id,
                target_worker,
            } => {
                events.push(TelemetryEvent::WakeEvent {
                    timestamp_nanos,
                    waker_task_id,
                    woken_task_id,
                    target_worker,
                });
            }
        }
        events
    }

    /// Called on file rotation — next reference to any id will re-emit its def.
    fn on_rotate(&mut self) {
        self.emitted_this_file.clear();
    }
}

/// Encapsulates all CPU-profiling flush state and logic.
///
/// This struct owns the cpu/sched profilers' drain-side state, thread-name and
/// callframe interning tables, and per-file emission tracking. By isolating
/// everything behind a single `#[cfg]`-gated type, the rest of
/// `TelemetryRecorder` stays free of cpu-profiling feature gates.
#[cfg(feature = "cpu-profiling")]
struct CpuFlushState {
    cpu_profiler: Option<crate::telemetry::cpu_profile::CpuProfiler>,
    /// When true, symbolicate callframe addresses and emit CallframeDef events.
    inline_callframe_symbols: bool,
    /// Addresses already symbolicated (across all files). Maps addr → (symbol, location).
    callframe_intern: HashMap<u64, (String, Option<String>)>,
    /// Addresses whose CallframeDef has been emitted in the current file.
    callframe_emitted_this_file: HashSet<u64>,
    /// tid → thread name, cached across files. Only populated for non-worker tids.
    thread_name_intern: HashMap<u32, String>,
    /// tids whose ThreadNameDef has been emitted in the current file.
    thread_name_emitted_this_file: HashSet<u32>,
}

#[cfg(feature = "cpu-profiling")]
impl CpuFlushState {
    fn new() -> Self {
        Self {
            cpu_profiler: None,
            inline_callframe_symbols: false,
            callframe_intern: HashMap::new(),
            callframe_emitted_this_file: HashSet::new(),
            thread_name_intern: HashMap::new(),
            thread_name_emitted_this_file: HashSet::new(),
        }
    }

    /// Called on file rotation — clear per-file tracking sets.
    fn on_rotate(&mut self) {
        self.callframe_emitted_this_file.clear();
        self.thread_name_emitted_this_file.clear();
    }

    /// Sync worker tid→id mappings, drain samples from both profilers, and
    /// write all CPU events (with their defs) directly to the writer.
    fn flush(
        &mut self,
        shared: &SharedState,
        writer: &mut dyn TraceWriter,
        flush_state: &mut FlushState,
    ) {
        self.sync_worker_tids(shared);
        self.flush_samples(writer, flush_state, shared);
    }

    /// Register worker tid→id mappings into both profilers.
    fn sync_worker_tids(&mut self, shared: &SharedState) {
        let tids = shared.worker_tids.lock().unwrap();
        if let Some(ref mut profiler) = self.cpu_profiler {
            for (&tid, &worker_id) in tids.iter() {
                profiler.register_worker(worker_id, tid);
            }
        }
        let mut shared_profiler = shared.sched_profiler.lock().unwrap();
        if let Some(ref mut profiler) = *shared_profiler {
            for (&tid, &worker_id) in tids.iter() {
                profiler.register_worker(worker_id, tid);
            }
        }
    }

    /// Drain samples from both profilers, cache thread names, and write events.
    fn flush_samples(
        &mut self,
        writer: &mut dyn TraceWriter,
        flush_state: &mut FlushState,
        shared: &SharedState,
    ) {
        // remove the profile, then reinsert it to allow splitting borrows
        if let Some(mut profiler) = self.cpu_profiler.take() {
            profiler.drain(|event, thread_name| {
                if let TelemetryEvent::CpuSample { tid, worker_id, .. } = &event
                    && *worker_id == UNKNOWN_WORKER
                    && !self.thread_name_intern.contains_key(tid)
                    && let Some(name) = thread_name
                {
                    self.thread_name_intern.insert(*tid, name.to_string());
                }
                self.write_cpu_event(&event, writer, flush_state);
            });
            self.cpu_profiler = Some(profiler);
        }
        {
            let mut shared_profiler = shared.sched_profiler.lock().unwrap();
            if let Some(ref mut profiler) = *shared_profiler {
                profiler.drain(|event| {
                    self.write_cpu_event(&event, writer, flush_state);
                });
            }
        }
    }

    /// Collect the prerequisite def events (ThreadNameDef / CallframeDef) for a
    /// CPU event, updating per-file tracking sets. The main event is appended
    /// last so the full batch can be written atomically.
    fn collect_cpu_event_batch(&mut self, event: &TelemetryEvent) -> Vec<TelemetryEvent> {
        let mut batch = Vec::new();
        // Emit ThreadNameDef for non-worker tids before their first sample in this file
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
        // Emit CallframeDef events for any new addresses in the callchain
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

    /// Write a single CPU event, emitting any necessary ThreadNameDef /
    /// CallframeDef events first. Uses `write_atomic` to guarantee all defs
    /// and the event itself land in the same file.
    fn write_cpu_event(
        &mut self,
        event: &TelemetryEvent,
        writer: &mut dyn TraceWriter,
        flush_state: &mut FlushState,
    ) {
        let batch = self.collect_cpu_event_batch(event);
        match writer.write_atomic(&batch) {
            Ok(WriteAtomicResult::Rotated) => {
                // write_atomic rotated — defs we just collected were written to
                // the old file. Clear per-file state and retry into the new file.
                debug_assert!(
                    writer.take_rotated(),
                    "write_atomic returned Rotated but take_rotated is false"
                );
                flush_state.on_rotate();
                self.on_rotate();
                let batch = self.collect_cpu_event_batch(event);
                let _ = writer.write_atomic(&batch);
            }
            Ok(WriteAtomicResult::Written | WriteAtomicResult::OversizedBatch) => {}
            Err(_) => {}
        }
    }
}

/// Intermediate layer between the recorder and the raw `TraceWriter`.
///
/// Owns the writer, spawn-location interning (`FlushState`), and CPU-profiling
/// flush state (`CpuFlushState`). Its API is roughly:
///
/// - `write_raw_event(raw)` — intern locations, handle rotation, write atomically
/// - `flush_cpu(shared)` — drain CPU/sched profilers into the trace
/// - `flush()` — flush the underlying writer
///
/// By isolating all write-path logic here, `TelemetryRecorder` only needs to
/// drain the collector and call into `EventWriter`, and tests can exercise the
/// write path without constructing a full recorder.
pub(crate) struct EventWriter {
    writer: Box<dyn TraceWriter>,
    flush_state: FlushState,
    #[cfg(feature = "cpu-profiling")]
    cpu_flush: Option<CpuFlushState>,
}

impl EventWriter {
    pub(crate) fn new(writer: Box<dyn TraceWriter>) -> Self {
        Self {
            writer,
            flush_state: FlushState::new(),
            #[cfg(feature = "cpu-profiling")]
            cpu_flush: None,
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
    ///
    /// Returns the outcome of the write:
    /// - `Written` — event was written successfully
    /// - `OversizedBatch` — event + defs too large for a single file, silently dropped
    /// - `Rotated` — should never happen (indicates double-rotation bug); caller should disable
    pub(crate) fn write_raw_event(&mut self, raw: RawEvent) -> std::io::Result<WriteAtomicResult> {
        if self.writer.take_rotated() {
            self.handle_rotation();
        }
        let events = self.flush_state.resolve(raw);
        match self.writer.write_atomic(&events)? {
            WriteAtomicResult::Written => Ok(WriteAtomicResult::Written),
            WriteAtomicResult::OversizedBatch => Ok(WriteAtomicResult::OversizedBatch),
            WriteAtomicResult::Rotated => {
                debug_assert!(
                    self.writer.take_rotated(),
                    "write atomic returned true, rotation occured"
                );
                self.handle_rotation();
                let events = self.flush_state.resolve(raw);
                match self.writer.write_atomic(&events)? {
                    r @ (WriteAtomicResult::Written | WriteAtomicResult::OversizedBatch) => Ok(r),
                    WriteAtomicResult::Rotated => {
                        eprintln!("double failed to write events. this is a bug. disabling");
                        Ok(WriteAtomicResult::Rotated)
                    }
                }
            }
        }
    }

    /// Drain CPU/sched profilers and write their events into the trace.
    #[cfg(feature = "cpu-profiling")]
    pub(crate) fn flush_cpu(&mut self, shared: &SharedState) {
        if let Some(mut cpu) = self.cpu_flush.take() {
            cpu.flush(shared, &mut *self.writer, &mut self.flush_state);
            self.cpu_flush = Some(cpu);
        }
    }

    pub(crate) fn flush(&mut self) -> std::io::Result<()> {
        self.writer.flush()
    }
}

pub struct TelemetryRecorder {
    shared: Arc<SharedState>,
    event_writer: EventWriter,
}

impl TelemetryRecorder {
    pub fn new(writer: Box<dyn TraceWriter>) -> Self {
        Self {
            shared: Arc::new(SharedState::new(Instant::now())),
            event_writer: EventWriter::new(writer),
        }
    }

    pub fn initialize(&mut self, handle: Handle) {
        let metrics = handle.metrics();
        self.shared.metrics.store(Arc::new(Some(metrics)));
    }

    fn flush(&mut self) {
        #[cfg(feature = "cpu-profiling")]
        self.event_writer.flush_cpu(&self.shared);

        for batch in self.shared.collector.drain() {
            for raw in batch {
                if let Ok(WriteAtomicResult::Rotated) = self.event_writer.write_raw_event(raw) {
                    self.shared.enabled.store(false, Ordering::Relaxed);
                    return;
                }
            }
        }
        self.event_writer.flush().unwrap();
    }

    pub(crate) fn install(
        builder: &mut tokio::runtime::Builder,
        writer: Box<dyn TraceWriter>,
        task_tracking_enabled: bool,
        start_time: Instant,
    ) -> Arc<Mutex<Self>> {
        let shared = Arc::new(SharedState::new(start_time));
        let recorder = Arc::new(Mutex::new(Self {
            shared: shared.clone(),
            event_writer: EventWriter::new(writer),
        }));

        let s1 = shared.clone();
        let s2 = shared.clone();
        let s3 = shared.clone();
        let s4 = shared.clone();

        builder
            .on_thread_park(move || {
                let event = s1.make_worker_park();
                s1.record_event(event);
            })
            .on_thread_unpark(move || {
                let event = s2.make_worker_unpark();
                s2.record_event(event);
            })
            .on_before_task_poll(move |meta| {
                let task_id = TaskId::from(meta.id());
                let location = meta.spawned_at();
                let event = s3.make_poll_start(location, task_id);
                s3.record_event(event);
            })
            .on_after_task_poll(move |_meta| {
                let event = s4.make_poll_end();
                s4.record_event(event);
            });

        if task_tracking_enabled {
            let s5 = shared.clone();
            builder.on_task_spawn(move |meta| {
                let task_id = TaskId::from(meta.id());
                let location = meta.spawned_at();
                s5.record_event(RawEvent::TaskSpawn { task_id, location });
            });
        }

        // Wire per-thread sched event tracking into worker lifecycle.
        #[cfg(feature = "cpu-profiling")]
        {
            let s_start = shared.clone();
            let s_stop = shared.clone();
            builder
                .on_thread_start(move || {
                    if let Ok(mut prof) = s_start.sched_profiler.lock()
                        && let Some(ref mut p) = *prof
                    {
                        let _ = p.track_current_thread();
                    }
                })
                .on_thread_stop(move || {
                    if let Ok(mut prof) = s_stop.sched_profiler.lock()
                        && let Some(ref mut p) = *prof
                    {
                        p.stop_tracking_current_thread();
                    }
                });
        }

        recorder
    }

    pub fn start_flush_task(
        recorder: Arc<Mutex<Self>>,
        interval: Duration,
    ) -> tokio::task::JoinHandle<()> {
        tokio::spawn(async move {
            let mut interval = tokio::time::interval(interval);
            loop {
                interval.tick().await;
                recorder.lock().unwrap().flush();
            }
        })
    }

    pub fn start_sampler_task(
        recorder: Arc<Mutex<Self>>,
        interval: Duration,
    ) -> tokio::task::JoinHandle<()> {
        let shared = recorder.lock().unwrap().shared.clone();
        tokio::spawn(async move {
            let mut tick = tokio::time::interval(interval);
            loop {
                tick.tick().await;
                let metrics_guard = shared.metrics.load();
                let Some(ref metrics) = **metrics_guard else {
                    continue;
                };
                shared.record_queue_sample(metrics.global_queue_depth());
            }
        })
    }
}

impl Drop for TelemetryRecorder {
    fn drop(&mut self) {
        self.flush();
    }
}

/// Cheap, cloneable handle for controlling telemetry from anywhere (e.g. a
/// file-watcher task running inside the runtime).
#[derive(Clone)]
pub struct TelemetryHandle {
    shared: Arc<SharedState>,
    recorder: Arc<Mutex<TelemetryRecorder>>,
}

impl TelemetryHandle {
    /// Start recording telemetry events.
    pub fn enable(&self) {
        self.shared.enabled.store(true, Ordering::Relaxed);
    }

    /// Stop recording telemetry events and flush any buffered data.
    pub fn disable(&self) {
        self.shared.enabled.store(false, Ordering::Relaxed);
        self.recorder.lock().unwrap().flush();
    }

    /// Get a handle for creating `Traced<F>` future wrappers.
    pub fn traced_handle(&self) -> crate::traced::TracedHandle {
        crate::traced::TracedHandle {
            shared: self.shared.clone(),
        }
    }

    /// Spawn a future wrapped in [`Traced`](crate::traced::Traced) for wake-event capture.
    #[track_caller]
    pub fn spawn<F>(&self, future: F) -> tokio::task::JoinHandle<F::Output>
    where
        F: std::future::Future + Send + 'static,
        F::Output: Send + 'static,
    {
        let traced_handle = self.traced_handle();
        tokio::spawn(async move {
            let task_id = tokio::task::try_id()
                .map(TaskId::from)
                .unwrap_or(TaskId::from_u32(0));
            crate::traced::Traced::new(future, traced_handle, task_id).await
        })
    }
}

/// RAII guard returned by [`TracedRuntimeBuilder::build`].
///
/// Dropping the guard signals the background flush/sampler thread to stop,
/// then performs a final flush.  The guard is independent of the runtime's
/// lifetime — drop the runtime whenever you like, then drop the guard.
pub struct TelemetryGuard {
    handle: TelemetryHandle,
    stop: Arc<AtomicBool>,
    thread: Option<std::thread::JoinHandle<()>>,
}

impl TelemetryGuard {
    /// Get a cheap, cloneable handle for controlling telemetry from other
    /// tasks or threads.
    pub fn handle(&self) -> TelemetryHandle {
        self.handle.clone()
    }

    /// The [`Instant`] at which trace recording began.
    ///
    /// All `timestamp_nanos` fields in [`TelemetryEvent`] are relative to this
    /// instant. Use `start_time.elapsed()` to produce timestamps in the same
    /// clock domain, or convert trace timestamps to wall-clock time via
    /// `start_time + Duration::from_nanos(event.timestamp_nanos)`.
    pub fn start_time(&self) -> Instant {
        self.handle.shared.start_time
    }

    /// Start recording telemetry events. Enabled by default when using [`TracedRuntime::build_and_start`].
    pub fn enable(&self) {
        self.handle.enable();
    }

    /// Stop recording telemetry events and flush any buffered data.
    pub fn disable(&self) {
        self.handle.disable();
    }
}

impl Drop for TelemetryGuard {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::Release);
        if let Some(t) = self.thread.take() {
            let _ = t.join();
        }
        self.handle.recorder.lock().unwrap().flush();
    }
}

pub struct TracedRuntimeBuilder {
    task_tracking_enabled: bool,
    #[cfg(feature = "cpu-profiling")]
    cpu_profiling_config: Option<crate::telemetry::cpu_profile::CpuProfilingConfig>,
    #[cfg(feature = "cpu-profiling")]
    sched_event_config: Option<crate::telemetry::cpu_profile::SchedEventConfig>,
    #[cfg(feature = "cpu-profiling")]
    inline_callframe_symbols: bool,
}

impl TracedRuntimeBuilder {
    /// Enables task tracking on the runtime
    ///
    /// This will cause the emitted events to include task tracking information (namely a TaskId and spawn location)
    pub fn with_task_tracking(mut self, enabled: bool) -> Self {
        self.task_tracking_enabled = enabled;
        self
    }

    /// Enable CPU profiling via perf_event_open. Captures stack traces at the configured
    /// frequency and merges them into the trace timeline.
    ///
    /// Requires the `cpu-profiling` feature and Linux with `perf_event_paranoid <= 2`.
    #[cfg(feature = "cpu-profiling")]
    pub fn with_cpu_profiling(
        mut self,
        config: crate::telemetry::cpu_profile::CpuProfilingConfig,
    ) -> Self {
        self.cpu_profiling_config = Some(config);
        self
    }

    /// Enable per-worker sched event capture (context switches).
    ///
    /// Opens a per-thread perf event fd on each worker thread via `on_thread_start`,
    /// capturing the stack at every context switch. Can be used alongside CPU profiling.
    ///
    /// Requires `perf_event_paranoid <= 1`
    #[cfg(feature = "cpu-profiling")]
    pub fn with_sched_events(
        mut self,
        config: crate::telemetry::cpu_profile::SchedEventConfig,
    ) -> Self {
        self.sched_event_config = Some(config);
        self
    }

    /// When enabled, callframe addresses from CPU samples are symbolicated inline
    /// and emitted as `CallframeDef` events in the trace. This allows reading the
    /// trace without needing to symbolicate offline.
    ///
    /// Requires the `cpu-profiling` feature.
    #[cfg(feature = "cpu-profiling")]
    pub fn with_inline_callframe_symbols(mut self, enabled: bool) -> Self {
        self.inline_callframe_symbols = enabled;
        self
    }

    pub fn build(
        self,
        mut builder: tokio::runtime::Builder,
        writer: Box<dyn TraceWriter>,
    ) -> std::io::Result<(tokio::runtime::Runtime, TelemetryGuard)> {
        // Capture both clock references at the same instant so that
        // perf timestamps (CLOCK_MONOTONIC) and trace-relative timestamps
        // (Instant::now()) share the same epoch.
        let start_instant = Instant::now();
        #[cfg(feature = "cpu-profiling")]
        let start_mono_ns = crate::telemetry::cpu_profile::clock_monotonic_ns();

        // Start CPU profiler if configured
        #[cfg(feature = "cpu-profiling")]
        let sampler = self
            .cpu_profiling_config
            .map(|config| crate::telemetry::cpu_profile::CpuProfiler::start(config, start_mono_ns));

        // Start sched event profiler if configured
        #[cfg(feature = "cpu-profiling")]
        let sched = self
            .sched_event_config
            .map(|config| crate::telemetry::cpu_profile::SchedProfiler::new(config, start_mono_ns));

        let recorder = TelemetryRecorder::install(
            &mut builder,
            writer,
            self.task_tracking_enabled,
            start_instant,
        );

        #[cfg(feature = "cpu-profiling")]
        {
            let mut rec = recorder.lock().unwrap();
            let mut cpu_flush = CpuFlushState::new();
            if let Some(Ok(sampler)) = sampler {
                cpu_flush.cpu_profiler = Some(sampler);
            }
            if let Some(Ok(sched)) = sched {
                *rec.shared.sched_profiler.lock().unwrap() = Some(sched);
            }
            cpu_flush.inline_callframe_symbols = self.inline_callframe_symbols;
            rec.event_writer.cpu_flush = Some(cpu_flush);
        }

        let runtime = builder.build()?;

        recorder
            .lock()
            .unwrap()
            .initialize(runtime.handle().clone());

        let stop = Arc::new(AtomicBool::new(false));

        let thread = {
            let rec = recorder.clone();
            let shared = recorder.lock().unwrap().shared.clone();
            let stop = stop.clone();
            std::thread::Builder::new()
                .name("telemetry-flush".into())
                .spawn(move || {
                    let flush_interval = Duration::from_millis(250);
                    let sample_interval = Duration::from_millis(10);
                    let mut last_flush = Instant::now();
                    let mut last_sample = Instant::now();

                    while !stop.load(Ordering::Acquire) {
                        std::thread::sleep(Duration::from_millis(5));

                        let now = Instant::now();
                        if now.duration_since(last_sample) >= sample_interval {
                            last_sample = now;
                            let metrics_guard = shared.metrics.load();
                            if let Some(ref metrics) = **metrics_guard {
                                shared.record_queue_sample(metrics.global_queue_depth());
                            }
                        }

                        if now.duration_since(last_flush) >= flush_interval {
                            last_flush = now;
                            rec.lock().unwrap().flush();
                        }
                    }
                })
                .expect("failed to spawn telemetry-flush thread")
        };

        let guard_shared = recorder.lock().unwrap().shared.clone();
        let guard = TelemetryGuard {
            handle: TelemetryHandle {
                shared: guard_shared,
                recorder,
            },
            stop,
            thread: Some(thread),
        };

        Ok((runtime, guard))
    }

    /// Build and immediately enable telemetry recording.
    pub fn build_and_start(
        self,
        builder: tokio::runtime::Builder,
        writer: Box<dyn TraceWriter>,
    ) -> std::io::Result<(tokio::runtime::Runtime, TelemetryGuard)> {
        let (runtime, guard) = self.build(builder, writer)?;
        guard.enable();
        Ok((runtime, guard))
    }
}

/// Entry point for setting up a traced Tokio runtime.
///
/// Use [`TracedRuntime::builder()`] for the full builder API, or [`TracedRuntime::build_and_start()`].
pub struct TracedRuntime;

impl TracedRuntime {
    /// Returns a builder for configuring the traced runtime.
    ///
    /// ```rust,ignore
    /// let (runtime, _guard) = TracedRuntime::builder()
    ///     .with_task_tracking(true)
    ///     .build_and_start(builder, Box::new(writer))?;
    /// ```
    pub fn builder() -> TracedRuntimeBuilder {
        TracedRuntimeBuilder {
            task_tracking_enabled: false,
            #[cfg(feature = "cpu-profiling")]
            cpu_profiling_config: None,
            #[cfg(feature = "cpu-profiling")]
            sched_event_config: None,
            #[cfg(feature = "cpu-profiling")]
            inline_callframe_symbols: false,
        }
    }

    /// Build a traced runtime with telemetry **disabled** by default.
    ///
    /// Call `.enable()` on the returned guard to start recording, or use
    /// [`build_and_start`](Self::build_and_start) to enable immediately.
    ///
    /// `builder` should already have `worker_threads` / `enable_all` configured
    /// but **not** yet built.  The guard owns the flush + sampler background
    /// thread; drop it after the runtime to get a clean final flush.
    ///
    /// ## Important Notes
    /// Installing `TracedRuntime` will override any value previously set for:
    /// - [`on_thread_park`](tokio::runtime::Builder::on_thread_park) — records worker park events
    /// - [`on_thread_unpark`](tokio::runtime::Builder::on_thread_unpark) — records worker unpark events
    /// - [`on_before_task_poll`](tokio::runtime::Builder::on_before_task_poll) — records poll-start events (with task ID and spawn location)
    /// - [`on_after_task_poll`](tokio::runtime::Builder::on_after_task_poll) — records poll-end events
    /// - [`on_task_spawn`](tokio::runtime::Builder::on_task_spawn) — records task spawn events (only when task tracking is enabled)
    /// - [`on_thread_start`](tokio::runtime::Builder::on_thread_start) — opens a per-thread perf event fd for sched-event capture (only with the `cpu-profiling` feature)
    /// - [`on_thread_stop`](tokio::runtime::Builder::on_thread_stop) — closes the per-thread perf event fd (only with the `cpu-profiling` feature)
    pub fn build(
        builder: tokio::runtime::Builder,
        writer: Box<dyn TraceWriter>,
    ) -> std::io::Result<(tokio::runtime::Runtime, TelemetryGuard)> {
        TracedRuntimeBuilder {
            task_tracking_enabled: false,
            #[cfg(feature = "cpu-profiling")]
            cpu_profiling_config: None,
            #[cfg(feature = "cpu-profiling")]
            sched_event_config: None,
            #[cfg(feature = "cpu-profiling")]
            inline_callframe_symbols: false,
        }
        .build(builder, writer)
    }

    /// Build and start runtime with all settings enabled.
    ///
    /// Future versions of this library MAY enable more features via this API as they are added.
    pub fn build_and_start(
        builder: tokio::runtime::Builder,
        writer: Box<dyn TraceWriter>,
    ) -> std::io::Result<(tokio::runtime::Runtime, TelemetryGuard)> {
        TracedRuntimeBuilder {
            task_tracking_enabled: true,
            #[cfg(feature = "cpu-profiling")]
            cpu_profiling_config: None,
            #[cfg(feature = "cpu-profiling")]
            sched_event_config: None,
            #[cfg(feature = "cpu-profiling")]
            inline_callframe_symbols: false,
        }
        .build_and_start(builder, writer)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::telemetry::writer::NullWriter;

    #[test]
    fn test_shared_state_no_spawn_location_fields() {
        // SharedState no longer has spawn location tracking fields.
        // All interning is in FlushState (flush thread local).
        let _recorder = TelemetryRecorder::new(Box::new(NullWriter));
        // Just verify it constructs without panic
    }

    #[test]
    fn test_flush_state_intern() {
        let mut fs = FlushState::new();
        #[track_caller]
        fn get_loc() -> &'static Location<'static> {
            Location::caller()
        }
        let loc = get_loc();
        let id1 = fs.intern(loc);
        let id2 = fs.intern(loc);
        assert_eq!(id1, id2);
        assert_ne!(id1.as_u16(), 0);
    }

    #[test]
    fn test_flush_state_on_rotate_clears_emitted() {
        let mut fs = FlushState::new();
        #[track_caller]
        fn get_loc() -> &'static Location<'static> {
            Location::caller()
        }
        let loc = get_loc();
        let id = fs.intern(loc);
        let mut defs = SmallVec::new();
        fs.collect_def(id, &mut defs);
        assert_eq!(defs.len(), 1);
        assert!(fs.emitted_this_file.contains(&id));
        fs.on_rotate();
        assert!(!fs.emitted_this_file.contains(&id));
    }

    /// Integration test: write events through the recorder with a rotating writer,
    /// then read back each file with TraceReader and verify every spawn location resolves.
    /// This is the key invariant: each rotated file is self-contained and readable.
    #[test]
    fn test_spawn_locations_resolve_after_rotation() {
        use crate::telemetry::analysis::TraceReader;

        let dir = tempfile::TempDir::new().unwrap();
        let base = dir.path().join("trace");

        // Two distinct call-sites → two different interned locations.
        #[track_caller]
        fn loc_a() -> &'static Location<'static> {
            Location::caller()
        }
        #[track_caller]
        fn loc_b() -> &'static Location<'static> {
            Location::caller()
        }
        let location_a = loc_a();
        let location_b = loc_b();

        let writer = crate::telemetry::writer::RotatingWriter::new(&base, 100, 100_000).unwrap();
        let mut ew = EventWriter::new(Box::new(writer));

        // Interleave PollStart and TaskSpawn so both code paths hit rotation.
        let locations = [
            location_a, location_b, location_a, location_b, location_a, location_b,
        ];
        for (i, loc) in locations.iter().enumerate() {
            let task_id = crate::telemetry::task_metadata::TaskId::from_u32(i as u32);
            ew.write_raw_event(RawEvent::TaskSpawn {
                task_id,
                location: loc,
            })
            .unwrap();
            ew.write_raw_event(RawEvent::PollStart {
                timestamp_nanos: (i as u64 + 1) * 1000,
                worker_id: 0,
                worker_local_queue_depth: 0,
                task_id,
                location: loc,
            })
            .unwrap();
        }
        ew.flush().unwrap();

        // Collect all rotated files.
        let mut files: Vec<_> = std::fs::read_dir(dir.path())
            .unwrap()
            .filter_map(|e| e.ok())
            .map(|e| e.path())
            .filter(|p| p.extension().map_or(false, |ext| ext == "bin"))
            .collect();
        files.sort();
        assert!(
            files.len() > 1,
            "expected multiple files from rotation, got {}",
            files.len()
        );

        let mut total_events = 0;
        for file in &files {
            let path = file.to_str().unwrap();
            let mut reader = TraceReader::new(path).unwrap();
            reader.read_header().unwrap();
            let events = reader.read_all().unwrap();

            // Every PollStart's spawn location must resolve to a real location string.
            for ev in &events {
                if let TelemetryEvent::PollStart { spawn_loc_id, .. } = ev {
                    let loc = reader.spawn_locations.get(spawn_loc_id).unwrap_or_else(|| {
                        panic!(
                            "file {path:?}: spawn_loc_id {spawn_loc_id:?} has no definition {:#?}",
                            reader.spawn_locations
                        )
                    });
                    assert!(
                        loc.contains(':'),
                        "location should be file:line:col, got {loc:?}"
                    );
                }
            }

            // Every TaskSpawn's spawn location must also resolve.
            for (task_id, spawn_loc_id) in &reader.task_spawn_locs {
                reader.spawn_locations.get(spawn_loc_id).unwrap_or_else(|| {
                    panic!("file {path:?}: task {task_id:?} spawn_loc_id {spawn_loc_id:?} has no definition")
                });
            }

            total_events += events.len();
        }
        assert_eq!(
            total_events, 6,
            "all PollStart events should be readable across files"
        );
    }

    // -----------------------------------------------------------------------
    // Proptest: stress-test rotation correctness across CPU + raw event
    // interleaving.
    //
    // The invariant under test: **every rotated file is self-contained**.
    //   • Every PollStart must have a preceding SpawnLocationDef in the same file.
    //   • Every CpuSample with worker_id == UNKNOWN_WORKER must have a preceding
    //     ThreadNameDef for its tid in the same file.
    //   • Every callchain address in a CpuSample must have a preceding
    //     CallframeDef in the same file (when inline_callframe_symbols is on).
    //   • No events are silently dropped.
    // -----------------------------------------------------------------------

    #[cfg(feature = "cpu-profiling")]
    mod rotation_proptest {
        use super::*;
        use crate::telemetry::analysis::TraceReader;
        use crate::telemetry::events::{CpuSampleSource, UNKNOWN_WORKER};
        use crate::telemetry::writer::RotatingWriter;
        use proptest::prelude::*;
        use std::collections::HashSet;

        /// A single operation in our simulated flush sequence.
        #[derive(Debug, Clone)]
        enum FlushOp {
            /// A CPU sample — optionally from a non-worker thread (which triggers
            /// ThreadNameDef emission) and with callchain addresses (which trigger
            /// CallframeDef emission).
            CpuSample {
                worker_id: usize,
                tid: u32,
                callchain: Vec<u64>,
            },
            /// A raw runtime event that carries a spawn-location reference.
            /// We use PollStart because it exercises SpawnLocationDef interning.
            PollStart {
                /// Index into a small set of distinct spawn locations.
                location_idx: usize,
            },
        }

        /// Generate a single flush operation.
        fn arb_flush_op() -> impl Strategy<Value = FlushOp> {
            prop_oneof![
                // CPU sample: 50% worker, 50% non-worker; 0..4 callchain addrs
                (
                    prop::bool::ANY,
                    0u32..4,
                    prop::collection::vec(0u64..8, 0..3),
                )
                    .prop_map(|(is_worker, tid, callchain)| {
                        FlushOp::CpuSample {
                            worker_id: if is_worker { 0 } else { UNKNOWN_WORKER },
                            tid,
                            callchain,
                        }
                    }),
                // PollStart referencing one of 3 distinct locations
                (0usize..3).prop_map(|idx| FlushOp::PollStart { location_idx: idx }),
            ]
        }

        /// A single flush round: some CPU events followed by some raw events,
        /// mirroring the real `TelemetryRecorder::flush` ordering.
        #[derive(Debug, Clone)]
        struct FlushRound {
            cpu_ops: Vec<FlushOp>,
            raw_ops: Vec<FlushOp>,
        }

        fn arb_flush_round() -> impl Strategy<Value = FlushRound> {
            (
                prop::collection::vec(arb_flush_op(), 0..12).prop_map(|ops| {
                    ops.into_iter()
                        .filter(|o| matches!(o, FlushOp::CpuSample { .. }))
                        .collect()
                }),
                prop::collection::vec(arb_flush_op(), 0..12).prop_map(|ops| {
                    ops.into_iter()
                        .filter(|o| matches!(o, FlushOp::PollStart { .. }))
                        .collect()
                }),
            )
                .prop_map(|(cpu_ops, raw_ops)| FlushRound { cpu_ops, raw_ops })
        }

        /// Execute one flush round, simulating the real
        /// `TelemetryRecorder::flush` sequence:
        ///   1. CPU events via EventWriter's cpu_flush
        ///   2. Raw events via EventWriter's write_raw_event
        ///
        /// `expected_raw` is incremented for each raw event that is actually
        /// written. CPU events may be silently dropped by `write_cpu_event`
        /// when the batch is oversized, so we don't attempt to predict their
        /// count — the self-containedness check in `verify_files` is the
        /// primary assertion for CPU correctness.
        fn execute_flush_round(
            round: &FlushRound,
            ew: &mut EventWriter,
            locations: &[&'static Location<'static>],
            timestamp: &mut u64,
            expected_raw: &mut usize,
        ) {
            // Phase 1: CPU flush — take cpu_flush out to allow split borrows
            if let Some(mut cpu) = ew.cpu_flush.take() {
                for op in &round.cpu_ops {
                    if let FlushOp::CpuSample {
                        worker_id,
                        tid,
                        callchain,
                    } = op
                    {
                        let event = TelemetryEvent::CpuSample {
                            timestamp_nanos: *timestamp,
                            worker_id: *worker_id,
                            tid: *tid,
                            source: CpuSampleSource::CpuProfile,
                            callchain: callchain.clone(),
                        };
                        *timestamp += 1;
                        cpu.write_cpu_event(&event, &mut *ew.writer, &mut ew.flush_state);
                    }
                }
                ew.cpu_flush = Some(cpu);
            }

            // Phase 2: raw event flush via EventWriter
            for op in &round.raw_ops {
                if let FlushOp::PollStart { location_idx } = op {
                    let loc = locations[*location_idx];
                    let task_id = TaskId::from_u32(*timestamp as u32);
                    let raw = RawEvent::PollStart {
                        timestamp_nanos: *timestamp,
                        worker_id: 0,
                        worker_local_queue_depth: 0,
                        task_id,
                        location: loc,
                    };
                    *timestamp += 1;

                    match ew.write_raw_event(raw).unwrap() {
                        WriteAtomicResult::Written => {
                            *expected_raw += 1;
                        }
                        WriteAtomicResult::OversizedBatch => {
                            // Batch won't ever fit — drop it.
                        }
                        WriteAtomicResult::Rotated => {
                            panic!("double rotation on raw event retry");
                        }
                    }
                }
            }
        }

        /// Read all .bin files from `dir`, verify each is self-contained, and
        /// return total PollStart event count.
        fn verify_files(dir: &std::path::Path) -> usize {
            let mut files: Vec<_> = std::fs::read_dir(dir)
                .unwrap()
                .filter_map(|e| e.ok())
                .map(|e| e.path())
                .filter(|p| p.extension().map_or(false, |ext| ext == "bin"))
                .collect();
            files.sort();

            let mut total_raw = 0;

            for file in &files {
                let path_str = file.to_str().unwrap();
                let mut reader = TraceReader::new(path_str)
                    .unwrap_or_else(|e| panic!("failed to open {path_str}: {e}"));
                reader.read_header().unwrap();

                // Read ALL events (including defs) so we can check ordering.
                let mut events = Vec::new();
                while let Some(ev) = reader.read_raw_event().unwrap() {
                    events.push(ev);
                }

                // Track which defs have appeared so far in this file.
                let mut spawn_loc_defs: HashSet<SpawnLocationId> = HashSet::new();
                let mut thread_name_defs: HashSet<u32> = HashSet::new();
                let mut callframe_defs: HashSet<u64> = HashSet::new();

                for ev in &events {
                    match ev {
                        TelemetryEvent::SpawnLocationDef { id, location } => {
                            assert!(
                                location.contains(':'),
                                "{path_str}: SpawnLocationDef has bad location: {location:?}"
                            );
                            spawn_loc_defs.insert(*id);
                        }
                        TelemetryEvent::ThreadNameDef { tid, .. } => {
                            thread_name_defs.insert(*tid);
                        }
                        TelemetryEvent::CallframeDef { address, .. } => {
                            callframe_defs.insert(*address);
                        }
                        TelemetryEvent::PollStart { spawn_loc_id, .. } => {
                            assert!(
                                spawn_loc_defs.contains(spawn_loc_id),
                                "{path_str}: PollStart references spawn_loc_id \
                                 {spawn_loc_id:?} but no SpawnLocationDef was emitted \
                                 in this file. Defs present: {spawn_loc_defs:?}"
                            );
                            total_raw += 1;
                        }
                        TelemetryEvent::CpuSample {
                            worker_id,
                            tid,
                            callchain,
                            ..
                        } => {
                            if *worker_id == UNKNOWN_WORKER {
                                assert!(
                                    thread_name_defs.contains(tid),
                                    "{path_str}: CpuSample for non-worker tid {tid} \
                                     has no ThreadNameDef in this file. \
                                     Defs present: {thread_name_defs:?}"
                                );
                            }
                            for addr in callchain {
                                assert!(
                                    callframe_defs.contains(addr),
                                    "{path_str}: CpuSample references callchain \
                                     address {addr:#x} but no CallframeDef in this \
                                     file. Defs present: {callframe_defs:?}"
                                );
                            }
                        }
                        _ => {}
                    }
                }
            }
            total_raw
        }

        proptest! {
            #![proptest_config(ProptestConfig::with_cases(256))]

            #[test]
            fn rotation_preserves_self_containedness(
                rounds in prop::collection::vec(arb_flush_round(), 1..6),
                max_file_size in 60u64..300,
            ) {
                let dir = tempfile::TempDir::new().unwrap();
                let base = dir.path().join("trace");

                let writer = RotatingWriter::new(&base, max_file_size, 1_000_000).unwrap();

                let mut ew = EventWriter::new(Box::new(writer));
                let mut cpu = CpuFlushState::new();
                // Enable callframe symbolication so CallframeDef paths are exercised.
                cpu.inline_callframe_symbols = true;
                // Pre-populate the thread-name intern table for non-worker tids 0..4
                // (in production these come from /proc; we just seed them here).
                for tid in 0u32..4 {
                    cpu.thread_name_intern.insert(tid, format!("thread-{tid}"));
                }
                ew.cpu_flush = Some(cpu);

                // Three distinct static locations to reference from PollStart events.
                #[track_caller]
                fn loc0() -> &'static Location<'static> { Location::caller() }
                #[track_caller]
                fn loc1() -> &'static Location<'static> { Location::caller() }
                #[track_caller]
                fn loc2() -> &'static Location<'static> { Location::caller() }
                let locations: Vec<&'static Location<'static>> = vec![loc0(), loc1(), loc2()];

                let mut timestamp = 1u64;
                let mut expected_raw = 0usize;

                for round in &rounds {
                    execute_flush_round(
                        round,
                        &mut ew,
                        &locations,
                        &mut timestamp,
                        &mut expected_raw,
                    );
                }
                ew.flush().unwrap();

                let actual_raw = verify_files(dir.path());

                // Raw events are tracked precisely — every written one must appear.
                prop_assert_eq!(
                    actual_raw, expected_raw,
                    "raw event count mismatch: expected {}, got {}", expected_raw, actual_raw
                );
                // CPU event counts are not checked here because write_cpu_event
                // silently drops oversized batches and doesn't return a result.
                // The self-containedness invariants in verify_files are the
                // primary assertion: every event that *does* appear must have
                // all its defs in the same file.
            }
        }
    }
}
