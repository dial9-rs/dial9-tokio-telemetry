use crate::telemetry::buffer::BUFFER;
use crate::telemetry::collector::CentralCollector;
use crate::telemetry::events::{EventType, MetricsSnapshot, SchedStat, TelemetryEvent};
use crate::telemetry::writer::TraceWriter;
use arc_swap::ArcSwap;
use std::cell::Cell;
use std::collections::HashMap;
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
    WORKER_ID.with(|cell| {
        if let Some(id) = cell.get() {
            return id;
        }
        let tid = std::thread::current().id();
        let map = worker_map.load();
        let id = map.get(&tid).copied().unwrap_or(0);
        cell.set(Some(id));
        id
    })
}

/// Invalidate the cached worker ID so it's re-resolved on next event.
fn invalidate_worker_id() {
    WORKER_ID.with(|cell| cell.set(None));
}

/// Shared state accessed lock-free by callbacks on the hot path.
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
    fn record_event(&self, event_type: EventType) {
        if !self.enabled.load(Ordering::Relaxed) {
            return;
        }
        let worker_id = resolve_worker_id(&self.worker_map);
        let metrics_guard = self.metrics.load();
        let worker_local_queue_depth = if let Some(ref metrics) = **metrics_guard {
            metrics.worker_local_queue_depth(worker_id)
        } else {
            0
        };

        let (cpu_time_nanos, sched_wait_delta_nanos) = match event_type {
            EventType::WorkerPark => {
                let cpu = crate::telemetry::events::thread_cpu_time_nanos();
                // Snapshot schedstat wait_time_ns for delta computation on unpark
                if let Ok(ss) = SchedStat::read_current() {
                    PARKED_SCHED_WAIT.with(|c| c.set(ss.wait_time_ns));
                }
                (cpu, 0)
            }
            EventType::WorkerUnpark => {
                let cpu = crate::telemetry::events::thread_cpu_time_nanos();
                let delta = if let Ok(ss) = SchedStat::read_current() {
                    let prev = PARKED_SCHED_WAIT.with(|c| c.get());
                    ss.wait_time_ns.saturating_sub(prev)
                } else {
                    0
                };
                (cpu, delta)
            }
            _ => (0, 0),
        };

        let event = TelemetryEvent::new(
            event_type,
            MetricsSnapshot {
                timestamp_nanos: self.start_time.elapsed().as_nanos() as u64,
                worker_id,
                global_queue_depth: 0,
                worker_local_queue_depth,
                cpu_time_nanos,
                sched_wait_delta_nanos,
            },
        );

        BUFFER.with(|buf| {
            let mut buf = buf.borrow_mut();
            buf.record_event(event);
            if buf.should_flush() || event_type == EventType::WorkerPark {
                self.collector.accept_flush(buf.flush());
                // Invalidate cached worker ID so it's re-resolved from the
                // latest map after the next rebuild.
                invalidate_worker_id();
            }
        });
    }
}

pub struct TelemetryRecorder {
    shared: Arc<SharedState>,
    writer: Box<dyn TraceWriter>,
}

impl TelemetryRecorder {
    pub fn new(writer: Box<dyn TraceWriter>) -> Self {
        Self {
            shared: Arc::new(SharedState {
                enabled: AtomicBool::new(false),
                collector: CentralCollector::new(),
                start_time: Instant::now(),
                metrics: ArcSwap::from_pointee(None),
                worker_map: ArcSwap::from_pointee(HashMap::new()),
            }),
            writer,
        }
    }

    pub fn initialize(&mut self, handle: Handle) {
        let metrics = handle.metrics();
        self.shared
            .worker_map
            .store(Arc::new(build_worker_map(&metrics)));
        self.shared.metrics.store(Arc::new(Some(metrics)));
    }

    fn flush(&mut self) {
        // Rebuild the ThreadId → worker index map so workers re-resolve on
        // their next event.
        let metrics_guard = self.shared.metrics.load();
        if let Some(ref metrics) = **metrics_guard {
            self.shared
                .worker_map
                .store(Arc::new(build_worker_map(metrics)));
        }

        for buffer in self.shared.collector.drain() {
            self.writer.write_batch(&buffer).unwrap();
        }
        self.writer.flush().unwrap();
    }

    pub fn install(
        builder: &mut tokio::runtime::Builder,
        writer: Box<dyn TraceWriter>,
    ) -> Arc<Mutex<Self>> {
        let recorder = Arc::new(Mutex::new(Self::new(writer)));
        let shared = recorder.lock().unwrap().shared.clone();

        let s1 = shared.clone();
        let s2 = shared.clone();
        let s3 = shared.clone();
        let s4 = shared;

        builder
            .on_thread_park(move || {
                s1.record_event(EventType::WorkerPark);
            })
            .on_thread_unpark(move || {
                s2.record_event(EventType::WorkerUnpark);
            })
            .on_before_task_poll(move |_meta| {
                s3.record_event(EventType::PollStart);
            })
            .on_after_task_poll(move |_meta| {
                s4.record_event(EventType::PollEnd);
            });

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
                let event = TelemetryEvent::new(
                    EventType::QueueSample,
                    MetricsSnapshot {
                        timestamp_nanos: ts,
                        worker_id: 0,
                        global_queue_depth: metrics.global_queue_depth(),
                        worker_local_queue_depth: 0,
                        cpu_time_nanos: 0,
                        sched_wait_delta_nanos: 0,
                    },
                );
                shared.collector.accept_flush(vec![event]);
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

/// RAII guard returned by [`TracedRuntime::build`].
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

    /// Start recording telemetry events. Enabled by default.
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

pub struct TracedRuntime;

impl TracedRuntime {
    /// Build a traced runtime with telemetry **disabled** by default.
    ///
    /// Call `.enable()` on the returned guard to start recording, or use
    /// [`build_and_start`](Self::build_and_start) to enable immediately.
    ///
    /// `builder` should already have `worker_threads` / `enable_all` configured
    /// but **not** yet built.  The guard owns the flush + sampler background
    /// thread; drop it after the runtime to get a clean final flush.
    pub fn build(
        mut builder: tokio::runtime::Builder,
        writer: Box<dyn TraceWriter>,
    ) -> std::io::Result<(tokio::runtime::Runtime, TelemetryGuard)> {
        let recorder = TelemetryRecorder::install(&mut builder, writer);
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
                                let event = TelemetryEvent::new(
                                    EventType::QueueSample,
                                    MetricsSnapshot {
                                        timestamp_nanos: ts,
                                        worker_id: 0,
                                        global_queue_depth: metrics.global_queue_depth(),
                                        worker_local_queue_depth: 0,
                                        cpu_time_nanos: 0,
                                        sched_wait_delta_nanos: 0,
                                    },
                                );
                                shared.collector.accept_flush(vec![event]);
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

    /// Build a traced runtime with telemetry **enabled** immediately.
    ///
    /// Equivalent to `build()` followed by `.enable()` on the guard.
    pub fn build_and_start(
        builder: tokio::runtime::Builder,
        writer: Box<dyn TraceWriter>,
    ) -> std::io::Result<(tokio::runtime::Runtime, TelemetryGuard)> {
        let (runtime, guard) = Self::build(builder, writer)?;
        guard.enable();
        Ok((runtime, guard))
    }
}
