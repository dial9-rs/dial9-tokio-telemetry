mod event_writer;
mod shared_state;

pub(crate) use shared_state::SharedState;

use event_writer::EventWriter;

use crate::metrics::{FlushMetrics, Operation};
use crate::telemetry::buffer;
use crate::telemetry::events::RawEvent;
use crate::telemetry::task_metadata::TaskId;
use crate::telemetry::writer::{RotatingWriter, TraceWriter};
use metrique::timers::Timer;
use std::path::PathBuf;
use std::sync::Arc;
use std::sync::atomic::Ordering;
use std::time::{Duration, Instant};

// ---------------------------------------------------------------------------
// Channel-based control for the flush thread
// ---------------------------------------------------------------------------

/// Commands sent to the flush thread from TelemetryHandle / TelemetryGuard.
enum ControlCommand {
    /// Flush, finalize (seal segment), then exit the thread.
    FinalizeAndStop(std::sync::mpsc::SyncSender<()>),
}

/// Stats returned by flush for metrics publishing.
// TODO: make this `#[metrics]` then flatten it
pub(crate) struct FlushStats {
    pub event_count: u64,
    pub dropped_batches: u64,
    pub cpu_time: Duration,
}

/// Perform one flush cycle: drain CPU profilers, drain the collector, write
/// events to disk, and flush the writer. This is the only code path that
/// touches EventWriter, and it runs exclusively on the flush thread.
fn flush_once(event_writer: &mut EventWriter, shared: &SharedState) -> FlushStats {
    let events_before = event_writer.events_written();
    let cpu_events_time = Instant::now();
    #[cfg(feature = "cpu-profiling")]
    {
        event_writer.flush_cpu(shared);
    }
    let cpu_time = cpu_events_time.elapsed();

    // Flush the current thread's buffer (the flush thread itself produces
    // queue-sample events via record_event) so those events reach the
    // collector before we drain it.
    buffer::drain_to_collector(&shared.collector);

    let dropped = shared.collector.take_dropped_batches();
    if dropped > 0 {
        tracing::warn!(
            dropped_batches = dropped,
            "telemetry flush fell behind, dropped batches"
        );
    }

    while let Some(batch) = shared.collector.next() {
        for raw in batch {
            if let Err(e) = event_writer.write_raw_event(raw) {
                tracing::warn!("failed to write trace event: {e}");
                shared.enabled.store(false, Ordering::Relaxed);
                return FlushStats {
                    event_count: event_writer.events_written() - events_before,
                    dropped_batches: dropped as u64,
                    cpu_time,
                };
            }
        }
    }
    if let Err(e) = event_writer.flush() {
        tracing::warn!("failed to flush trace data: {e}");
    }
    FlushStats {
        event_count: event_writer.events_written() - events_before,
        dropped_batches: dropped as u64,
        cpu_time,
    }
}

/// Install telemetry hooks on the runtime builder. Returns the shared state
/// (for the hot-path callbacks) and the event writer (to be moved into the
/// flush thread). No shared mutex is created.
fn install_hooks(
    builder: &mut tokio::runtime::Builder,
    writer: Box<dyn TraceWriter>,
    task_tracking_enabled: bool,
    start_time_ns: u64,
) -> (Arc<SharedState>, EventWriter) {
    let shared = Arc::new(SharedState::new(start_time_ns));

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
            s5.record_event(RawEvent::TaskSpawn {
                timestamp_nanos: crate::telemetry::events::clock_monotonic_ns(),
                task_id,
                location,
            });
        });
        let s6 = shared.clone();
        builder.on_task_terminate(move |meta| {
            let task_id = TaskId::from(meta.id());
            s6.record_event(RawEvent::TaskTerminate {
                timestamp_nanos: crate::telemetry::events::clock_monotonic_ns(),
                task_id,
            });
        });
    }

    #[cfg(feature = "cpu-profiling")]
    {
        let s_start = shared.clone();
        let s_stop = shared.clone();
        builder
            .on_thread_start(move || {
                // Register as Blocking initially; worker threads will
                // overwrite this to Worker(i) in resolve_worker_id.
                {
                    let tid = crate::telemetry::events::current_tid();
                    s_start
                        .thread_roles
                        .lock()
                        .unwrap()
                        .insert(tid, crate::telemetry::events::ThreadRole::Blocking);
                }
                if let Ok(mut prof) = s_start.sched_profiler.lock()
                    && let Some(ref mut p) = *prof
                {
                    let _ = p.track_current_thread();
                }
            })
            .on_thread_stop(move || {
                {
                    let tid = crate::telemetry::events::current_tid();
                    s_stop.thread_roles.lock().unwrap().remove(&tid);
                }
                if let Ok(mut prof) = s_stop.sched_profiler.lock()
                    && let Some(ref mut p) = *prof
                {
                    p.stop_tracking_current_thread();
                }
            });
    }

    let event_writer = EventWriter::new(writer);
    (shared, event_writer)
}

/// Cheap, cloneable handle for controlling telemetry from anywhere.
///
/// Uses a channel to communicate with the flush thread — no shared mutex.
#[derive(Clone)]
pub struct TelemetryHandle {
    shared: Arc<SharedState>,
    control_tx: std::sync::mpsc::SyncSender<ControlCommand>,
}

impl TelemetryHandle {
    pub fn enable(&self) {
        self.shared.enabled.store(true, Ordering::Relaxed);
    }

    pub fn disable(&self) {
        self.shared.enabled.store(false, Ordering::Relaxed);
    }

    pub fn traced_handle(&self) -> crate::traced::TracedHandle {
        crate::traced::TracedHandle {
            shared: self.shared.clone(),
        }
    }

    #[track_caller]
    pub fn spawn<F>(&self, future: F) -> tokio::task::JoinHandle<F::Output>
    where
        F: std::future::Future + Send + 'static,
        F::Output: Send + 'static,
    {
        let traced_handle = self.traced_handle();
        tokio::spawn(async move {
            let task_id = tokio::task::try_id().map(TaskId::from).unwrap_or_default();
            crate::traced::Traced::new(future, traced_handle, task_id).await
        })
    }
}

/// Holds the background worker thread and its stop signal.
struct WorkerHandle {
    shutdown: Option<tokio::sync::oneshot::Sender<Duration>>,
    thread: Option<std::thread::JoinHandle<()>>,
}

/// RAII guard returned by [`TracedRuntimeBuilder::build`].
pub struct TelemetryGuard {
    handle: TelemetryHandle,
    flush_thread: Option<std::thread::JoinHandle<()>>,
    worker: Option<WorkerHandle>,
}

impl TelemetryGuard {
    pub fn handle(&self) -> TelemetryHandle {
        self.handle.clone()
    }

    pub fn start_time(&self) -> u64 {
        self.handle.shared.start_time_ns
    }

    pub fn enable(&self) {
        self.handle.enable();
    }

    pub fn disable(&self) {
        self.handle.disable();
    }

    /// Send FinalizeAndStop to the flush thread, join it, then drain the
    /// caller's thread-local buffer into the collector so the flush thread
    /// picks up any stragglers.
    fn stop_flush_thread(&mut self) {
        // Drain the current thread's buffer (e.g. main thread in block_on)
        // which may contain TaskSpawn events that were never flushed.
        buffer::drain_to_collector(&self.handle.shared.collector);

        // Tell the flush thread to do a final flush + finalize, then exit.
        let (ack_tx, ack_rx) = std::sync::mpsc::sync_channel(0);
        if self
            .handle
            .control_tx
            .send(ControlCommand::FinalizeAndStop(ack_tx))
            .is_ok()
        {
            let _ = ack_rx.recv();
        }
        if let Some(t) = self.flush_thread.take() {
            let _ = t.join();
        }
    }

    /// Flush remaining events, seal the final segment, and wait for the
    /// worker to drain. Returns `Ok(())` if the worker finishes within the
    /// timeout, `Err` if it times out or the worker panics.
    ///
    /// Consumes the guard so `Drop` becomes a no-op.
    pub async fn graceful_shutdown(mut self, timeout: Duration) -> Result<(), std::io::Error> {
        tracing::debug!(target: "dial9_telemetry", "graceful_shutdown starting");

        // 1. Stop flush thread (flushes + finalizes the last segment)
        self.stop_flush_thread();
        tracing::debug!(target: "dial9_telemetry", "flush thread joined, segment sealed");

        // 2. Signal worker to drain with the given timeout and wait
        if let Some(ref mut w) = self.worker {
            tracing::debug!(target: "dial9_telemetry", timeout_secs = timeout.as_secs(), "waiting for worker drain");
            if let Some(tx) = w.shutdown.take() {
                let _ = tx.send(timeout);
            }
            if let Some(t) = w.thread.take() {
                let _ = tokio::task::spawn_blocking(move || t.join()).await;
            }
            tracing::debug!(target: "dial9_telemetry", "worker finished");
        }

        Ok(())
    }
}

impl Drop for TelemetryGuard {
    fn drop(&mut self) {
        // 1. Stop the flush thread (flushes + finalizes)
        self.stop_flush_thread();
        // 2. Hard shutdown: drop the sender without sending — worker sees
        // RecvError and exits without draining. No need to join the thread.
        // For graceful drain, use graceful_shutdown() instead.
        if let Some(ref mut w) = self.worker {
            w.shutdown.take();
        }
    }
}

/// Marker: no trace path has been set yet.
pub struct NoTracePath;
/// Marker: a trace path has been set.
pub struct HasTracePath;

pub struct TracedRuntimeBuilder<P = NoTracePath> {
    enabled: bool,
    task_tracking_enabled: bool,
    trace_path: Option<PathBuf>,
    #[cfg(feature = "cpu-profiling")]
    cpu_profiling_config: Option<crate::telemetry::cpu_profile::CpuProfilingConfig>,
    #[cfg(feature = "cpu-profiling")]
    sched_event_config: Option<crate::telemetry::cpu_profile::SchedEventConfig>,
    #[cfg(feature = "worker-s3")]
    s3_config: Option<crate::background_task::s3::S3Config>,
    #[cfg(feature = "worker-s3")]
    s3_client: Option<aws_sdk_s3::Client>,
    worker_poll_interval: Option<Duration>,
    worker_metrics_sink: Option<metrique_writer::BoxEntrySink>,
    _marker: std::marker::PhantomData<P>,
}

// Methods available on both NoTracePath and HasTracePath.
impl<P> TracedRuntimeBuilder<P> {
    /// Set to `false` to build a plain runtime with no telemetry
    /// installed and a dummy [`TelemetryGuard`]. Defaults to `true`.
    ///
    /// Unlike [`TelemetryGuard::enable`]/[`TelemetryGuard::disable`]
    /// (which toggle recording at runtime), this controls whether
    /// telemetry hooks and threads are installed at all.
    pub fn install(mut self, enabled: bool) -> Self {
        self.enabled = enabled;
        self
    }

    pub fn with_task_tracking(mut self, enabled: bool) -> Self {
        self.task_tracking_enabled = enabled;
        self
    }

    #[cfg(feature = "cpu-profiling")]
    pub fn with_cpu_profiling(
        mut self,
        config: crate::telemetry::cpu_profile::CpuProfilingConfig,
    ) -> Self {
        self.cpu_profiling_config = Some(config);
        self
    }

    #[cfg(feature = "cpu-profiling")]
    pub fn with_sched_events(
        mut self,
        config: crate::telemetry::cpu_profile::SchedEventConfig,
    ) -> Self {
        self.sched_event_config = Some(config);
        self
    }

    /// Configure S3 upload for sealed trace segments.
    #[cfg(feature = "worker-s3")]
    pub fn with_s3_uploader(mut self, config: crate::background_task::s3::S3Config) -> Self {
        self.s3_config = Some(config);
        self
    }

    /// Provide a pre-built S3 client (for custom credentials or endpoints).
    #[cfg(feature = "worker-s3")]
    pub fn with_s3_client(mut self, client: aws_sdk_s3::Client) -> Self {
        self.s3_client = Some(client);
        self
    }

    pub fn with_worker_poll_interval(mut self, interval: Duration) -> Self {
        self.worker_poll_interval = Some(interval);
        self
    }

    pub fn with_worker_metrics_sink(mut self, sink: metrique_writer::BoxEntrySink) -> Self {
        self.worker_metrics_sink = Some(sink);
        self
    }

    fn into_state<Q>(self) -> TracedRuntimeBuilder<Q> {
        TracedRuntimeBuilder {
            enabled: self.enabled,
            task_tracking_enabled: self.task_tracking_enabled,
            trace_path: self.trace_path,
            #[cfg(feature = "cpu-profiling")]
            cpu_profiling_config: self.cpu_profiling_config,
            #[cfg(feature = "cpu-profiling")]
            sched_event_config: self.sched_event_config,
            #[cfg(feature = "worker-s3")]
            s3_config: self.s3_config,
            #[cfg(feature = "worker-s3")]
            s3_client: self.s3_client,
            worker_poll_interval: self.worker_poll_interval,
            worker_metrics_sink: self.worker_metrics_sink,
            _marker: std::marker::PhantomData,
        }
    }
}

impl TracedRuntimeBuilder<NoTracePath> {
    /// Set the trace output path. This transitions the builder to
    /// `HasTracePath`, enabling `build()` and `build_and_start()`.
    pub fn with_trace_path(
        mut self,
        path: impl Into<PathBuf>,
    ) -> TracedRuntimeBuilder<HasTracePath> {
        self.trace_path = Some(path.into());
        self.into_state()
    }

    /// Build with a custom writer (for tests or `NullWriter`).
    /// No background worker is spawned.
    pub fn build_with_writer(
        self,
        builder: tokio::runtime::Builder,
        writer: impl TraceWriter + 'static,
    ) -> std::io::Result<(tokio::runtime::Runtime, TelemetryGuard)> {
        self.into_state::<HasTracePath>()
            .build_inner(builder, Box::new(writer))
    }

    /// Build with a custom writer and immediately enable recording.
    pub fn build_and_start_with_writer(
        self,
        builder: tokio::runtime::Builder,
        writer: impl TraceWriter + 'static,
    ) -> std::io::Result<(tokio::runtime::Runtime, TelemetryGuard)> {
        let (runtime, guard) = self.build_with_writer(builder, writer)?;
        guard.enable();
        Ok((runtime, guard))
    }

    /// Build the traced runtime. No background worker is spawned
    /// (use `with_trace_path()` first for worker support).
    pub fn build(
        self,
        builder: tokio::runtime::Builder,
        writer: impl TraceWriter + 'static,
    ) -> std::io::Result<(tokio::runtime::Runtime, TelemetryGuard)> {
        self.build_with_writer(builder, writer)
    }

    /// Build and immediately enable recording.
    pub fn build_and_start(
        self,
        builder: tokio::runtime::Builder,
        writer: impl TraceWriter + 'static,
    ) -> std::io::Result<(tokio::runtime::Runtime, TelemetryGuard)> {
        self.build_and_start_with_writer(builder, writer)
    }
}

impl TracedRuntimeBuilder<HasTracePath> {
    /// Set the trace output path (no-op, already set).
    pub fn with_trace_path(mut self, path: impl Into<PathBuf>) -> Self {
        self.trace_path = Some(path.into());
        self
    }

    /// Build the traced runtime with a `RotatingWriter`.
    ///
    /// The background worker is auto-spawned when cpu-profiling or S3 is
    /// configured. Recording starts disabled; call [`TelemetryGuard::enable`]
    /// to begin, or use [`build_and_start`](Self::build_and_start).
    pub fn build(
        self,
        builder: tokio::runtime::Builder,
        writer: RotatingWriter,
    ) -> std::io::Result<(tokio::runtime::Runtime, TelemetryGuard)> {
        self.build_inner(builder, Box::new(writer))
    }

    /// Build the traced runtime and immediately enable recording.
    pub fn build_and_start(
        self,
        builder: tokio::runtime::Builder,
        writer: RotatingWriter,
    ) -> std::io::Result<(tokio::runtime::Runtime, TelemetryGuard)> {
        let (runtime, guard) = self.build(builder, writer)?;
        guard.enable();
        Ok((runtime, guard))
    }

    /// Build with a custom writer (for tests). The background worker is
    /// still spawned if cpu-profiling or S3 is configured and `trace_path`
    /// is set.
    pub fn build_with_writer(
        self,
        builder: tokio::runtime::Builder,
        writer: impl TraceWriter + 'static,
    ) -> std::io::Result<(tokio::runtime::Runtime, TelemetryGuard)> {
        self.build_inner(builder, Box::new(writer))
    }

    /// Build with a custom writer and immediately enable recording.
    pub fn build_and_start_with_writer(
        self,
        builder: tokio::runtime::Builder,
        writer: impl TraceWriter + 'static,
    ) -> std::io::Result<(tokio::runtime::Runtime, TelemetryGuard)> {
        let (runtime, guard) = self.build_with_writer(builder, writer)?;
        guard.enable();
        Ok((runtime, guard))
    }

    fn build_inner(
        self,
        mut builder: tokio::runtime::Builder,
        writer: Box<dyn TraceWriter>,
    ) -> std::io::Result<(tokio::runtime::Runtime, TelemetryGuard)> {
        if !self.enabled {
            return TracedRuntime::build_disabled(builder);
        }
        let start_mono_ns = crate::telemetry::events::clock_monotonic_ns();

        #[cfg(feature = "cpu-profiling")]
        let sampler = self
            .cpu_profiling_config
            .as_ref()
            .map(|c| crate::telemetry::cpu_profile::CpuProfiler::start(c.clone()));

        #[cfg(feature = "cpu-profiling")]
        let sched = self
            .sched_event_config
            .map(crate::telemetry::cpu_profile::SchedProfiler::new);

        let (shared, mut event_writer) = install_hooks(
            &mut builder,
            writer,
            self.task_tracking_enabled,
            start_mono_ns,
        );

        #[cfg(feature = "cpu-profiling")]
        {
            if let Some(Ok(sampler)) = sampler {
                event_writer.cpu_profiler = Some(sampler);
            }
            if let Some(Ok(sched)) = sched {
                *shared.sched_profiler.lock().unwrap() = Some(sched);
            }
        }

        let runtime = builder.build()?;

        // Store runtime metrics so worker-id resolution works.
        shared
            .metrics
            .store(Arc::new(Some(runtime.handle().metrics())));

        // Channel for TelemetryHandle/Guard → flush thread communication.
        // Bounded(1) so senders don't pile up commands.
        let (control_tx, control_rx) = std::sync::mpsc::sync_channel::<ControlCommand>(1);

        let thread = {
            let shared = shared.clone();
            let flush_metrics_sink = self
                .worker_metrics_sink
                .clone()
                .unwrap_or_else(metrique_writer::sink::DevNullSink::boxed);
            std::thread::Builder::new()
                .name("dial9-flush".into())
                .spawn(move || {
                    // Lower this thread's scheduling priority so it doesn't
                    // compete with worker threads for CPU time.
                    // SAFETY: nice() is a simple syscall with no memory safety
                    // implications. Increasing the nice value (lowering priority)
                    // is always permitted for unprivileged processes.
                    #[cfg(target_os = "linux")]
                    unsafe {
                        let _ = libc::nice(10);
                    }

                    let sample_interval = Duration::from_millis(10);
                    let mut last_sample = Instant::now();

                    loop {
                        let mut ack_tx = None;
                        let mut exit = false;
                        // wait for control commands up to 5ms.
                        match control_rx.recv_timeout(Duration::from_millis(5)) {
                            Ok(ControlCommand::FinalizeAndStop(ack)) => {
                                ack_tx = Some(ack);
                                exit = true;
                            }
                            Err(std::sync::mpsc::RecvTimeoutError::Disconnected) => {
                                // All senders dropped — do a best-effort finalize.
                                exit = true;
                            }
                            Err(std::sync::mpsc::RecvTimeoutError::Timeout) => {}
                        }

                        let now = Instant::now();
                        if now.duration_since(last_sample) >= sample_interval {
                            last_sample = now;
                            let metrics_guard = shared.metrics.load();
                            if let Some(ref metrics) = **metrics_guard {
                                shared.record_queue_sample(metrics.global_queue_depth());
                            }
                        }

                        let mut flush_timer = Timer::start_now();
                        let stats = flush_once(&mut event_writer, &shared);
                        flush_timer.stop();
                        if stats.event_count > 0 || stats.dropped_batches > 0 {
                            let _guard = FlushMetrics {
                                operation: Operation::Flush,
                                event_count: stats.event_count,
                                flush_duration: flush_timer,
                                dropped_batches: stats.dropped_batches,
                                cpu_flush_duration: stats.cpu_time,
                                last_flush: exit,
                            }
                            .append_on_drop(flush_metrics_sink.clone());
                        }
                        if exit && let Err(e) = event_writer.finalize() {
                            tracing::warn!("failed to finalize trace segment: {e}");
                        }
                        if let Some(tx) = ack_tx.take() {
                            let _ = tx.send(());
                        }
                        if exit {
                            return;
                        }
                    }
                })
                .expect("failed to spawn telemetry-flush thread")
        };

        // Auto-construct worker config when we have a trace path and
        // either cpu-profiling or S3 is configured.
        let worker_config = self.trace_path.and_then(|trace_path| {
            #[allow(unused_mut)]
            let mut needs_worker = false;
            #[allow(unused_mut)]
            let mut symbolize = false;

            #[cfg(feature = "cpu-profiling")]
            if self.cpu_profiling_config.is_some() {
                needs_worker = true;
                symbolize = true;
            }

            #[cfg(feature = "worker-s3")]
            let s3 = self.s3_config;
            #[cfg(feature = "worker-s3")]
            if s3.is_some() {
                needs_worker = true;
            }

            if !needs_worker {
                return None;
            }

            let poll_interval = self
                .worker_poll_interval
                .unwrap_or(crate::background_task::DEFAULT_POLL_INTERVAL);
            let metrics_sink = self
                .worker_metrics_sink
                .unwrap_or_else(metrique_writer::sink::DevNullSink::boxed);

            let config = crate::background_task::BackgroundTaskConfig::builder()
                .trace_path(trace_path)
                .poll_interval(poll_interval)
                .symbolize(symbolize)
                .metrics_sink(metrics_sink);

            #[cfg(feature = "worker-s3")]
            let config = config.maybe_s3(s3).maybe_client(self.s3_client);

            Some(config.build())
        });

        #[allow(unused_mut)]
        let mut worker = None;
        if let Some(config) = worker_config {
            let (shutdown_tx, shutdown_rx) = tokio::sync::oneshot::channel();
            let wt = std::thread::Builder::new()
                .name("dial9-worker".into())
                .spawn(move || {
                    crate::background_task::run_background_task(config, shutdown_rx);
                })
                .expect("failed to spawn dial9-worker thread");
            worker = Some(WorkerHandle {
                shutdown: Some(shutdown_tx),
                thread: Some(wt),
            });
        }

        let guard = TelemetryGuard {
            handle: TelemetryHandle { shared, control_tx },
            flush_thread: Some(thread),
            worker,
        };

        Ok((runtime, guard))
    }
}

/// Entry point for setting up a traced Tokio runtime.
pub struct TracedRuntime;

impl TracedRuntime {
    pub fn builder() -> TracedRuntimeBuilder<NoTracePath> {
        TracedRuntimeBuilder {
            enabled: true,
            task_tracking_enabled: false,
            trace_path: None,
            #[cfg(feature = "cpu-profiling")]
            cpu_profiling_config: None,
            #[cfg(feature = "cpu-profiling")]
            sched_event_config: None,
            #[cfg(feature = "worker-s3")]
            s3_config: None,
            #[cfg(feature = "worker-s3")]
            s3_client: None,
            worker_poll_interval: None,
            worker_metrics_sink: None,
            _marker: std::marker::PhantomData,
        }
    }

    /// Build a plain runtime with no telemetry installed.
    pub fn build_disabled(
        mut builder: tokio::runtime::Builder,
    ) -> std::io::Result<(tokio::runtime::Runtime, TelemetryGuard)> {
        let runtime = builder.build()?;
        let shared = Arc::new(SharedState::new(
            crate::telemetry::events::clock_monotonic_ns(),
        ));
        // Create a dummy channel — nothing listens on the other end, so
        // disable() sends will fail silently (which is correct for disabled).
        let (control_tx, _control_rx) = std::sync::mpsc::sync_channel(1);
        let guard = TelemetryGuard {
            handle: TelemetryHandle { shared, control_tx },
            flush_thread: None,
            worker: None,
        };
        Ok((runtime, guard))
    }

    /// Build the traced runtime. Recording starts disabled.
    pub fn build(
        builder: tokio::runtime::Builder,
        writer: impl TraceWriter + 'static,
    ) -> std::io::Result<(tokio::runtime::Runtime, TelemetryGuard)> {
        Self::builder().build_with_writer(builder, writer)
    }

    /// Build the traced runtime and immediately enable recording.
    pub fn build_and_start(
        builder: tokio::runtime::Builder,
        writer: impl TraceWriter + 'static,
    ) -> std::io::Result<(tokio::runtime::Runtime, TelemetryGuard)> {
        Self::builder().build_and_start_with_writer(builder, writer)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::telemetry::NullWriter;
    use std::panic::Location;

    #[test]
    fn test_shared_state_no_spawn_location_fields() {
        let _shared = SharedState::new(crate::telemetry::events::clock_monotonic_ns());
    }

    #[test]
    fn build_disabled_produces_working_runtime_with_noop_guard() {
        let builder = tokio::runtime::Builder::new_multi_thread();
        let (runtime, guard) = TracedRuntime::builder()
            .install(false)
            .build(builder, NullWriter)
            .unwrap();

        // Guard methods should be safe no-ops
        guard.enable();
        guard.disable();
        let handle = guard.handle();
        let _start = guard.start_time();

        // Runtime should work normally, including handle.spawn
        runtime.block_on(async {
            let result = tokio::spawn(async { 42 }).await.unwrap();
            assert_eq!(result, 42);

            let traced = handle.spawn(async { 7 }).await.unwrap();
            assert_eq!(traced, 7);
        });

        // No flush thread or worker to join
        assert!(guard.flush_thread.is_none());
        assert!(guard.worker.is_none());
    }

    #[test]
    fn test_spawn_locations_resolve_after_rotation() {
        use crate::telemetry::analysis::TraceReader;
        use crate::telemetry::format::WorkerId;

        let dir = tempfile::TempDir::new().unwrap();
        let base = dir.path().join("trace");

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

        let writer = crate::telemetry::writer::RotatingWriter::builder()
            .base_path(&base)
            .max_file_size(100)
            .max_total_size(100_000)
            .build()
            .unwrap();
        let mut ew = EventWriter::new(Box::new(writer));

        let locations = [
            location_a, location_b, location_a, location_b, location_a, location_b,
        ];
        for (i, loc) in locations.iter().enumerate() {
            let task_id = crate::telemetry::task_metadata::TaskId::from_u32(i as u32);
            ew.write_raw_event(RawEvent::TaskSpawn {
                timestamp_nanos: (i as u64 + 1) * 1000,
                task_id,
                location: loc,
            })
            .unwrap();
            ew.write_raw_event(RawEvent::PollStart {
                timestamp_nanos: (i as u64 + 1) * 1000,
                worker_id: WorkerId::from(0usize),
                worker_local_queue_depth: 0,
                task_id,
                location: loc,
            })
            .unwrap();
        }
        ew.flush().unwrap();
        ew.finalize().unwrap();

        let mut files: Vec<_> = std::fs::read_dir(dir.path())
            .unwrap()
            .filter_map(|e| e.ok())
            .map(|e| e.path())
            .filter(|p| p.extension().is_some_and(|ext| ext == "bin"))
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

            for (spawn_loc, loc) in &reader.spawn_locations {
                assert!(
                    loc.contains(':'),
                    "location should be file:line:col, got {loc:?} for {spawn_loc:?}"
                );
            }

            for (task_id, spawn_loc) in &reader.task_spawn_locs {
                reader.spawn_locations.get(spawn_loc).unwrap_or_else(|| {
                    panic!(
                        "file {path:?}: task {task_id:?} spawn_loc {spawn_loc:?} has no definition"
                    )
                });
            }

            let events = reader.read_all().unwrap();
            total_events += events.len();
        }
        assert_eq!(
            total_events, 6,
            "all PollStart events should be readable across files"
        );
    }

    #[cfg(feature = "cpu-profiling")]
    mod rotation_proptest {
        use super::*;
        use crate::telemetry::analysis::TraceReader;
        use crate::telemetry::events::{CpuSampleData, CpuSampleSource, TelemetryEvent};
        use crate::telemetry::format::WorkerId;
        use crate::telemetry::task_metadata::TaskId;
        use crate::telemetry::writer::RotatingWriter;
        use proptest::prelude::*;

        #[derive(Debug, Clone)]
        enum FlushOp {
            CpuSample {
                worker_id: WorkerId,
                tid: u32,
                callchain: Vec<u64>,
            },
            PollStart {
                location_idx: usize,
            },
        }

        fn arb_flush_op() -> impl Strategy<Value = FlushOp> {
            prop_oneof![
                (
                    prop::bool::ANY,
                    0u32..4,
                    prop::collection::vec(0u64..8, 0..3),
                )
                    .prop_map(|(is_worker, tid, callchain)| {
                        FlushOp::CpuSample {
                            worker_id: if is_worker {
                                WorkerId::from(0usize)
                            } else {
                                WorkerId::UNKNOWN
                            },
                            tid,
                            callchain,
                        }
                    }),
                (0usize..3).prop_map(|idx| FlushOp::PollStart { location_idx: idx }),
            ]
        }

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

        fn execute_flush_round(
            round: &FlushRound,
            ew: &mut EventWriter,
            locations: &[&'static Location<'static>],
            timestamp: &mut u64,
            expected_raw: &mut usize,
        ) {
            for op in &round.cpu_ops {
                if let FlushOp::CpuSample {
                    worker_id,
                    tid,
                    callchain,
                } = op
                {
                    let data = CpuSampleData {
                        timestamp_nanos: *timestamp,
                        worker_id: *worker_id,
                        tid: *tid,
                        source: CpuSampleSource::CpuProfile,
                        thread_name: None,
                        callchain: callchain.clone(),
                    };
                    *timestamp += 1;
                    ew.write_cpu_event(&data);
                }
            }

            for op in &round.raw_ops {
                if let FlushOp::PollStart { location_idx } = op {
                    let loc = locations[*location_idx];
                    let task_id = TaskId::from_u32(*timestamp as u32);
                    let raw = RawEvent::PollStart {
                        timestamp_nanos: *timestamp,
                        worker_id: WorkerId::from(0usize),
                        worker_local_queue_depth: 0,
                        task_id,
                        location: loc,
                    };
                    *timestamp += 1;

                    ew.write_raw_event(raw).unwrap();
                    *expected_raw += 1;
                }
            }
        }

        fn verify_files(dir: &std::path::Path) -> usize {
            let mut files: Vec<_> = std::fs::read_dir(dir)
                .unwrap()
                .filter_map(|e| e.ok())
                .map(|e| e.path())
                .filter(|p| p.extension().is_some_and(|ext| ext == "bin"))
                .collect();
            files.sort();

            let mut total_raw = 0;

            for file in &files {
                let path_str = file.to_str().unwrap();
                let reader = TraceReader::new(path_str)
                    .unwrap_or_else(|e| panic!("failed to open {path_str}: {e}"));

                // In the new format, spawn locations come from the string pool.
                // Verify every PollStart's spawn_loc_id resolves.
                let spawn_locs = &reader.spawn_locations;

                for ev in &reader.events {
                    match ev {
                        TelemetryEvent::PollStart { spawn_loc, .. } => {
                            assert!(
                                spawn_locs.contains_key(spawn_loc),
                                "{path_str}: PollStart references spawn_loc {spawn_loc:?} but no definition in this file. Defs: {spawn_locs:?}"
                            );
                            total_raw += 1;
                        }
                        TelemetryEvent::CpuSample { .. } => {
                            // Callchain addresses are raw; symbolization
                            // happens in the background worker now.
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

                let writer = RotatingWriter::builder()
                    .base_path(&base)
                    .max_file_size(max_file_size)
                    .max_total_size(1_000_000)
                    .build()
                    .unwrap();

                let mut ew = EventWriter::new(Box::new(writer));

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
                ew.finalize().unwrap();

                let actual_raw = verify_files(dir.path());

                prop_assert_eq!(
                    actual_raw, expected_raw,
                    "raw event count mismatch: expected {}, got {}", expected_raw, actual_raw
                );
            }
        }
    }
}
