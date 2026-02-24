use crate::telemetry::buffer::BUFFER;
use crate::telemetry::collector::CentralCollector;
use crate::telemetry::events::{RawEvent, SchedStat, TelemetryEvent};
use crate::telemetry::task_metadata::{SpawnLocationId, TaskId};
use crate::telemetry::writer::TraceWriter;
use arc_swap::ArcSwap;
use smallvec::SmallVec;
use std::cell::Cell;
use std::collections::{HashMap, HashSet};
use std::panic::Location;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};
use tokio::runtime::{Handle, RuntimeMetrics};

/// Sentinel value for events from non-worker threads
const UNKNOWN_WORKER: usize = 255;

thread_local! {
    /// Cached tokio worker index for this thread. `None` means not yet resolved.
    /// Once resolved, the worker ID is stable for the lifetime of the thread—a thread
    /// won't become a *different* worker, though it may stop being a worker entirely.
    static WORKER_ID: Cell<Option<usize>> = const { Cell::new(None) };
    /// Whether we've already emitted a WorkerTid for this thread.
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
/// On first resolution, also sends a WorkerTid event so the flush thread can
/// map OS tids to worker IDs for CPU profiling.
fn resolve_worker_id(
    metrics: &ArcSwap<Option<RuntimeMetrics>>,
    shared: Option<&SharedState>,
) -> Option<usize> {
    WORKER_ID.with(|cell| {
        if let Some(id) = cell.get() {
            return Some(id);
        }
        let tid = std::thread::current().id();
        if let Some(ref m) = **metrics.load() {
            for i in 0..m.num_workers() {
                if m.worker_thread_id(i) == Some(tid) {
                    cell.set(Some(i));
                    // Emit WorkerTid once per thread
                    if let Some(shared) = shared {
                        TID_EMITTED.with(|emitted| {
                            if !emitted.get() {
                                emitted.set(true);
                                let os_tid = crate::telemetry::events::current_tid();
                                shared.record_event(RawEvent::WorkerTid {
                                    worker_id: i,
                                    tid: os_tid,
                                });
                            }
                        });
                    }
                    return Some(i);
                }
            }
        }
        None
    })
}

/// Shared state accessed lock-free by callbacks on the hot path.
/// No spawn location tracking here — all interning happens in the flush thread.
struct SharedState {
    enabled: AtomicBool,
    collector: CentralCollector,
    start_time: Instant,
    metrics: ArcSwap<Option<RuntimeMetrics>>,
}

impl SharedState {
    fn new() -> Self {
        Self {
            enabled: AtomicBool::new(false),
            collector: CentralCollector::new(),
            start_time: Instant::now(),
            metrics: ArcSwap::from_pointee(None),
        }
    }

    fn record_event(&self, event: RawEvent) {
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
            RawEvent::WorkerTid { .. } => {
                // Internal-only event, not serialized. Handled by flush() for CPU profiling.
            }
        }
        events
    }

    /// Called on file rotation — next reference to any id will re-emit its def.
    fn on_rotate(&mut self) {
        self.emitted_this_file.clear();
    }
}

pub struct TelemetryRecorder {
    shared: Arc<SharedState>,
    writer: Box<dyn TraceWriter>,
    flush_state: FlushState,
    #[cfg(feature = "cpu-profiling")]
    cpu_profiler: Option<crate::telemetry::cpu_profile::CpuProfiler>,
    /// When true, symbolicate callframe addresses and emit CallframeDef events.
    #[cfg(feature = "cpu-profiling")]
    inline_callframe_symbols: bool,
    /// Addresses already symbolicated (across all files). Maps addr → symbol name.
    #[cfg(feature = "cpu-profiling")]
    callframe_intern: HashMap<u64, String>,
    /// Addresses whose CallframeDef has been emitted in the current file.
    #[cfg(feature = "cpu-profiling")]
    callframe_emitted_this_file: HashSet<u64>,
}

impl TelemetryRecorder {
    pub fn new(writer: Box<dyn TraceWriter>) -> Self {
        Self {
            shared: Arc::new(SharedState::new()),
            writer,
            flush_state: FlushState::new(),
            #[cfg(feature = "cpu-profiling")]
            cpu_profiler: None,
            #[cfg(feature = "cpu-profiling")]
            inline_callframe_symbols: false,
            #[cfg(feature = "cpu-profiling")]
            callframe_intern: HashMap::new(),
            #[cfg(feature = "cpu-profiling")]
            callframe_emitted_this_file: HashSet::new(),
        }
    }

    pub fn initialize(&mut self, handle: Handle) {
        let metrics = handle.metrics();
        self.shared.metrics.store(Arc::new(Some(metrics)));
    }

    fn flush(&mut self) {
        for batch in self.shared.collector.drain() {
            for raw in batch {
                // Update CPU profiler's tid→worker mapping from WorkerThreadMap events
                #[cfg(feature = "cpu-profiling")]
                if let RawEvent::WorkerTid { worker_id, tid } = &raw {
                    if let Some(ref mut profiler) = self.cpu_profiler {
                        profiler.register_worker(*worker_id, *tid);
                    }
                }
                self.write_raw_event(raw).unwrap();
            }
        }
        // Drain CPU profiler samples and write them
        #[cfg(feature = "cpu-profiling")]
        {
            let has_profiler = self.cpu_profiler.is_some();
            if let Some(ref mut profiler) = self.cpu_profiler {
                let cpu_events = profiler.drain();
                for event in &cpu_events {
                    if self.inline_callframe_symbols {
                        if let TelemetryEvent::CpuSample { callchain, .. } = event {
                            // Check for rotation before writing defs
                            if self.writer.take_rotated() {
                                self.flush_state.on_rotate();
                                self.callframe_emitted_this_file.clear();
                            }
                            for &addr in callchain {
                                if !self.callframe_emitted_this_file.contains(&addr) {
                                    // Symbolicate if not yet interned
                                    if !self.callframe_intern.contains_key(&addr) {
                                        let sym = perf_self_profile::resolve_symbol(addr);
                                        let name =
                                            sym.name.unwrap_or_else(|| format!("{:#x}", addr));
                                        self.callframe_intern.insert(addr, name);
                                    }
                                    let symbol = self.callframe_intern[&addr].clone();
                                    let def = TelemetryEvent::CallframeDef {
                                        address: addr,
                                        symbol,
                                    };
                                    let _ = self.writer.write_event(&def);
                                    self.callframe_emitted_this_file.insert(addr);
                                }
                            }
                        }
                    }
                    let _ = self.writer.write_event(event);
                }
            }
            if has_profiler {
                // only log once to confirm profiler is wired up
                static LOGGED: std::sync::atomic::AtomicBool =
                    std::sync::atomic::AtomicBool::new(false);
                if !LOGGED.swap(true, Ordering::Relaxed) {
                    eprintln!(
                        "[cpu-profiler] profiler active in flush path, has_pending={}",
                        self.cpu_profiler.as_ref().unwrap().has_pending()
                    );
                }
            }
        }
        self.writer.flush().unwrap();
    }

    /// Convert a RawEvent to wire format, interning locations as needed.
    fn write_raw_event(&mut self, raw: RawEvent) -> std::io::Result<()> {
        if self.writer.take_rotated() {
            self.flush_state.on_rotate();
            #[cfg(feature = "cpu-profiling")]
            self.callframe_emitted_this_file.clear();
        }
        let events = self.flush_state.resolve(raw);
        if self.writer.write_atomic(&events)? {
            // write_atomic rotated to a new file — defs referenced by these
            // events were only in the old file. Emit them into the new file.
            debug_assert!(
                self.writer.take_rotated(),
                "write atomic returned true, rotation occured"
            );
            self.flush_state.on_rotate();
            #[cfg(feature = "cpu-profiling")]
            self.callframe_emitted_this_file.clear();
            let events = self.flush_state.resolve(raw);
            if self.writer.write_atomic(&events)? {
                // Something bad is happening...
                eprintln!("double failed to write events. this is a bug. disabling");
                self.shared.enabled.store(false, Ordering::Relaxed);
            };
        }
        Ok(())
    }

    pub(crate) fn install(
        builder: &mut tokio::runtime::Builder,
        writer: Box<dyn TraceWriter>,
        task_tracking_enabled: bool,
    ) -> Arc<Mutex<Self>> {
        let shared = Arc::new(SharedState::new());
        let recorder = Arc::new(Mutex::new(Self {
            shared: shared.clone(),
            writer,
            flush_state: FlushState::new(),
            #[cfg(feature = "cpu-profiling")]
            cpu_profiler: None,
            #[cfg(feature = "cpu-profiling")]
            inline_callframe_symbols: false,
            #[cfg(feature = "cpu-profiling")]
            callframe_intern: HashMap::new(),
            #[cfg(feature = "cpu-profiling")]
            callframe_emitted_this_file: HashSet::new(),
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
                let ts = shared.start_time.elapsed().as_nanos() as u64;
                shared.collector.accept_flush(vec![RawEvent::QueueSample {
                    timestamp_nanos: ts,
                    global_queue_depth: metrics.global_queue_depth(),
                }]);
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
        // Start CPU profiler if configured
        #[cfg(feature = "cpu-profiling")]
        let sampler = if let Some(config) = self.cpu_profiling_config {
            let mono_ns = crate::telemetry::cpu_profile::clock_monotonic_ns();
            Some(crate::telemetry::cpu_profile::CpuProfiler::start(
                config, mono_ns,
            ))
        } else {
            None
        };

        let recorder = TelemetryRecorder::install(&mut builder, writer, self.task_tracking_enabled);

        #[cfg(feature = "cpu-profiling")]
        if let Some(Ok(sampler)) = sampler {
            recorder.lock().unwrap().cpu_profiler = Some(sampler);
            recorder.lock().unwrap().inline_callframe_symbols = self.inline_callframe_symbols;
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
                            if !shared.enabled.load(Ordering::Relaxed) {
                                continue;
                            }
                            let metrics_guard = shared.metrics.load();
                            if let Some(ref metrics) = **metrics_guard {
                                let ts = shared.start_time.elapsed().as_nanos() as u64;
                                shared.collector.accept_flush(vec![RawEvent::QueueSample {
                                    timestamp_nanos: ts,
                                    global_queue_depth: metrics.global_queue_depth(),
                                }]);
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
/// Use [`TracedRuntime::builder()`] for the full builder API, or
/// [`TracedRuntime::build_and_start()`] for the simple one-liner (backwards-compatible).
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
    pub fn build(
        builder: tokio::runtime::Builder,
        writer: Box<dyn TraceWriter>,
    ) -> std::io::Result<(tokio::runtime::Runtime, TelemetryGuard)> {
        TracedRuntimeBuilder {
            task_tracking_enabled: false,
            #[cfg(feature = "cpu-profiling")]
            cpu_profiling_config: None,
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
        let mut recorder = TelemetryRecorder::new(Box::new(writer));

        // Interleave PollStart and TaskSpawn so both code paths hit rotation.
        let locations = [
            location_a, location_b, location_a, location_b, location_a, location_b,
        ];
        for (i, loc) in locations.iter().enumerate() {
            let task_id = crate::telemetry::task_metadata::TaskId::from_u32(i as u32);
            recorder
                .write_raw_event(RawEvent::TaskSpawn {
                    task_id,
                    location: loc,
                })
                .unwrap();
            recorder
                .write_raw_event(RawEvent::PollStart {
                    timestamp_nanos: (i as u64 + 1) * 1000,
                    worker_id: 0,
                    worker_local_queue_depth: 0,
                    task_id,
                    location: loc,
                })
                .unwrap();
        }
        recorder.writer.flush().unwrap();

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
}
