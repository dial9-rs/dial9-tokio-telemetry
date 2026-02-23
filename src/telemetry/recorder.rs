use crate::telemetry::buffer::BUFFER;
use crate::telemetry::collector::CentralCollector;
use crate::telemetry::events::{RawEvent, SchedStat, TelemetryEvent};
use crate::telemetry::task_metadata::{SpawnLocationId, TaskId};
use crate::telemetry::writer::TraceWriter;
use arc_swap::ArcSwap;
use std::cell::Cell;
use std::collections::{HashMap, HashSet};
use std::panic::Location;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::thread::ThreadId;
use std::time::{Duration, Instant};
use tokio::runtime::{Handle, RuntimeMetrics};

thread_local! {
    /// Cached tokio worker index for this thread. `None` means not yet resolved.
    static WORKER_ID: Cell<Option<usize>> = const { Cell::new(None) };
    /// schedstat wait_time_ns captured at park time, used to compute delta on unpark.
    static PARKED_SCHED_WAIT: Cell<u64> = const { Cell::new(0) };
}

/// Build a ThreadId → tokio worker index map from RuntimeMetrics.
fn build_worker_map(metrics: &RuntimeMetrics) -> HashMap<ThreadId, usize> {
    let mut map = HashMap::new();
    for i in 0..metrics.num_workers() {
        if let Some(tid) = metrics.worker_thread_id(i) {
            map.insert(tid, i);
        }
    }
    map
}

/// Resolve the current thread's tokio worker index, caching in TLS.
/// Falls back to 0 if the map isn't populated yet.
fn resolve_worker_id(worker_map: &ArcSwap<HashMap<ThreadId, usize>>) -> usize {
    // TODO: should return Option<usize> instead
    WORKER_ID.with(|cell| {
        if let Some(id) = cell.get() {
            return id;
        }
        let tid = std::thread::current().id();
        let map = worker_map.load();
        let id = map.get(&tid).copied().unwrap_or(0);
        if id != 0 || map.contains_key(&tid) {
            cell.set(Some(id));
        }
        id
    })
}

/// Invalidate the cached worker ID so it's re-resolved on next event.
fn invalidate_worker_id() {
    WORKER_ID.with(|cell| cell.set(None));
}

/// Shared state accessed lock-free by callbacks on the hot path.
/// No spawn location tracking here — all interning happens in the flush thread.
struct SharedState {
    enabled: AtomicBool,
    collector: CentralCollector,
    start_time: Instant,
    metrics: ArcSwap<Option<RuntimeMetrics>>,
    /// ThreadId → tokio worker index, rebuilt every flush cycle.
    /// Uses ArcSwap for lock-free reads on hot path (cached in TLS).
    /// Must rebuild periodically because worker threads can restart with new ThreadIds.
    /// Clone cost is negligible: ~100ns for typical instances, max ~1µs on very large instances (100s of workers), every 250ms.
    worker_map: ArcSwap<HashMap<ThreadId, usize>>,
}

impl SharedState {
    fn new() -> Self {
        Self {
            enabled: AtomicBool::new(false),
            collector: CentralCollector::new(),
            start_time: Instant::now(),
            metrics: ArcSwap::from_pointee(None),
            worker_map: ArcSwap::from_pointee(HashMap::new()),
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
                invalidate_worker_id();
            }
        });
    }

    fn make_poll_start(&self, location: &'static Location<'static>, task_id: TaskId) -> RawEvent {
        let worker_id = resolve_worker_id(&self.worker_map);
        let metrics_guard = self.metrics.load();
        let worker_local_queue_depth = if let Some(ref metrics) = **metrics_guard {
            metrics.worker_local_queue_depth(worker_id)
        } else {
            0
        };
        RawEvent::PollStart {
            timestamp_nanos: self.start_time.elapsed().as_nanos() as u64,
            worker_id,
            worker_local_queue_depth,
            task_id,
            location,
        }
    }

    fn make_poll_end(&self) -> RawEvent {
        let worker_id = resolve_worker_id(&self.worker_map);
        RawEvent::PollEnd {
            timestamp_nanos: self.start_time.elapsed().as_nanos() as u64,
            worker_id,
        }
    }

    fn make_worker_park(&self) -> RawEvent {
        let worker_id = resolve_worker_id(&self.worker_map);
        let metrics_guard = self.metrics.load();
        let worker_local_queue_depth = if let Some(ref metrics) = **metrics_guard {
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
            worker_id,
            worker_local_queue_depth,
            cpu_time_nanos,
        }
    }

    fn make_worker_unpark(&self) -> RawEvent {
        let worker_id = resolve_worker_id(&self.worker_map);
        let metrics_guard = self.metrics.load();
        let worker_local_queue_depth = if let Some(ref metrics) = **metrics_guard {
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
            worker_id,
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

    /// Ensure a SpawnLocationDef has been written for this id in the current file.
    fn ensure_def(
        &mut self,
        id: SpawnLocationId,
        writer: &mut dyn TraceWriter,
    ) -> std::io::Result<()> {
        if self.emitted_this_file.insert(id) {
            let loc = self.intern_strings[id.as_u16() as usize].clone();
            writer.write_event(&TelemetryEvent::SpawnLocationDef { id, location: loc })?;
        }
        Ok(())
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
}

impl TelemetryRecorder {
    pub fn new(writer: Box<dyn TraceWriter>) -> Self {
        Self {
            shared: Arc::new(SharedState::new()),
            writer,
            flush_state: FlushState::new(),
        }
    }

    pub fn initialize(&mut self, handle: Handle) {
        let metrics = handle.metrics();
        self.shared.metrics.store(Arc::new(Some(metrics)));
    }

    fn flush(&mut self) {
        let metrics_guard = self.shared.metrics.load();
        if let Some(ref metrics) = **metrics_guard {
            self.shared
                .worker_map
                .store(Arc::new(build_worker_map(metrics)));
        }

        for batch in self.shared.collector.drain() {
            for raw in batch {
                self.write_raw_event(raw).unwrap();
            }
        }
        self.writer.flush().unwrap();
    }

    /// Convert a RawEvent to wire format, interning locations as needed.
    fn write_raw_event(&mut self, raw: RawEvent) -> std::io::Result<()> {
        match raw {
            RawEvent::TaskSpawn { task_id, location } => {
                let spawn_loc_id = self.flush_state.intern(location);
                self.flush_state
                    .ensure_def(spawn_loc_id, &mut *self.writer)?;
                if self.writer.take_rotated() {
                    self.flush_state.on_rotate();
                }
                self.writer.write_event(&TelemetryEvent::TaskSpawn {
                    task_id,
                    spawn_loc_id,
                })?;
                if self.writer.take_rotated() {
                    self.flush_state.on_rotate();
                }
            }
            RawEvent::PollStart {
                timestamp_nanos,
                worker_id,
                worker_local_queue_depth,
                task_id,
                location,
            } => {
                let spawn_loc_id = self.flush_state.intern(location);
                self.flush_state
                    .ensure_def(spawn_loc_id, &mut *self.writer)?;
                if self.writer.take_rotated() {
                    self.flush_state.on_rotate();
                }
                self.writer.write_event(&TelemetryEvent::PollStart {
                    timestamp_nanos,
                    worker_id,
                    worker_local_queue_depth,
                    task_id,
                    spawn_loc_id,
                })?;
                if self.writer.take_rotated() {
                    self.flush_state.on_rotate();
                }
            }
            RawEvent::PollEnd {
                timestamp_nanos,
                worker_id,
            } => {
                self.writer.write_event(&TelemetryEvent::PollEnd {
                    timestamp_nanos,
                    worker_id,
                })?;
                if self.writer.take_rotated() {
                    self.flush_state.on_rotate();
                }
            }
            RawEvent::WorkerPark {
                timestamp_nanos,
                worker_id,
                worker_local_queue_depth,
                cpu_time_nanos,
            } => {
                self.writer.write_event(&TelemetryEvent::WorkerPark {
                    timestamp_nanos,
                    worker_id,
                    worker_local_queue_depth,
                    cpu_time_nanos,
                })?;
                if self.writer.take_rotated() {
                    self.flush_state.on_rotate();
                }
            }
            RawEvent::WorkerUnpark {
                timestamp_nanos,
                worker_id,
                worker_local_queue_depth,
                cpu_time_nanos,
                sched_wait_delta_nanos,
            } => {
                self.writer.write_event(&TelemetryEvent::WorkerUnpark {
                    timestamp_nanos,
                    worker_id,
                    worker_local_queue_depth,
                    cpu_time_nanos,
                    sched_wait_delta_nanos,
                })?;
                if self.writer.take_rotated() {
                    self.flush_state.on_rotate();
                }
            }
            RawEvent::QueueSample {
                timestamp_nanos,
                global_queue_depth,
            } => {
                self.writer.write_event(&TelemetryEvent::QueueSample {
                    timestamp_nanos,
                    global_queue_depth,
                })?;
                if self.writer.take_rotated() {
                    self.flush_state.on_rotate();
                }
            }
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
        }));

        let s0 = shared.clone();
        let s1 = shared.clone();
        let s2 = shared.clone();
        let s3 = shared.clone();
        let s4 = shared.clone();

        builder
            .on_thread_start(move || {
                let metrics_guard = s0.metrics.load();
                if let Some(ref metrics) = **metrics_guard {
                    s0.worker_map.store(Arc::new(build_worker_map(metrics)));
                }
            })
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
}

impl TracedRuntimeBuilder {
    pub fn with_task_tracking(mut self, enabled: bool) -> Self {
        self.task_tracking_enabled = enabled;
        self
    }

    pub fn build(
        self,
        mut builder: tokio::runtime::Builder,
        writer: Box<dyn TraceWriter>,
    ) -> std::io::Result<(tokio::runtime::Runtime, TelemetryGuard)> {
        let recorder = TelemetryRecorder::install(&mut builder, writer, self.task_tracking_enabled);
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
        }
        .build(builder, writer)
    }

    /// Build a traced runtime with telemetry **enabled** immediately, with task metadata tracking on.
    ///
    /// This is the backwards-compatible one-liner. Equivalent to:
    /// `TracedRuntime::builder().with_task_tracking(true).build_and_start(builder, writer)`
    pub fn build_and_start(
        builder: tokio::runtime::Builder,
        writer: Box<dyn TraceWriter>,
    ) -> std::io::Result<(tokio::runtime::Runtime, TelemetryGuard)> {
        TracedRuntimeBuilder {
            task_tracking_enabled: true,
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
        let mut writer = NullWriter;
        #[track_caller]
        fn get_loc() -> &'static Location<'static> {
            Location::caller()
        }
        let loc = get_loc();
        let id = fs.intern(loc);
        fs.ensure_def(id, &mut writer).unwrap();
        assert!(fs.emitted_this_file.contains(&id));
        fs.on_rotate();
        assert!(!fs.emitted_this_file.contains(&id));
    }
}
