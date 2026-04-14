mod event_writer;
mod runtime_context;
mod shared_state;

pub(crate) use runtime_context::{RuntimeContext, current_worker_id};
pub(crate) use shared_state::SharedState;

use event_writer::EventWriter;
use runtime_context::{make_poll_end, make_poll_start, make_worker_park, make_worker_unpark};

use crate::metrics::{FlushMetrics, Operation, TlDrainMetrics};
use crate::telemetry::buffer;
use crate::telemetry::events::RawEvent;
use crate::telemetry::task_metadata::TaskId;
use crate::telemetry::writer::{RotatingWriter, TraceWriter};
use metrique::timers::Timer;
use metrique::unit::Microsecond;
use metrique::unit_of_work::metrics;
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
#[metrics(subfield, rename_all = "PascalCase")]
#[derive(Debug)]
pub(crate) struct FlushStats {
    pub event_count: u64,
    pub dropped_batches: u64,
    #[metrics(unit = Microsecond)]
    pub cpu_flush_duration: Duration,
}

/// Perform one flush cycle: drain CPU profilers, drain the collector, write
/// events to disk, and flush the writer. This is the only code path that
/// touches EventWriter, and it runs exclusively on the flush thread.
fn flush_once(
    event_writer: &mut EventWriter,
    shared: &SharedState,
    drain_self: bool,
) -> FlushStats {
    let events_before = event_writer.events_written();
    let cpu_events_time = Instant::now();
    #[cfg(feature = "cpu-profiling")]
    {
        event_writer.flush_cpu(shared);
    }
    let cpu_flush_duration = cpu_events_time.elapsed();

    if drain_self {
        // Periodically flush the flush thread's own TL buffer (queue samples + CPU events).
        // We don't drain every cycle because each batch becomes its own trace segment;
        // batching ~1s worth avoids writing tiny segments every 5ms.
        buffer::drain_to_collector(&shared.collector);
    }

    let dropped = shared.collector.take_dropped_batches();
    if dropped > 0 {
        tracing::warn!(
            dropped_batches = dropped,
            "telemetry flush fell behind, dropped batches"
        );
    }

    while let Some(batch) = shared.collector.next() {
        if batch.event_count > 0
            && let Err(e) = event_writer.write_encoded_batch(&batch)
        {
            tracing::warn!("failed to transcode batch: {e}");
            shared.enabled.store(false, Ordering::Relaxed);
            return FlushStats {
                event_count: event_writer.events_written() - events_before,
                dropped_batches: dropped as u64,
                cpu_flush_duration,
            };
        }
    }
    if let Err(e) = event_writer.flush() {
        tracing::warn!("failed to flush trace data: {e}");
    }
    FlushStats {
        event_count: event_writer.events_written() - events_before,
        dropped_batches: dropped as u64,
        cpu_flush_duration,
    }
}

/// Register telemetry callbacks on a runtime builder.
/// Closures capture `Arc<RuntimeContext>` (runtime-specific) and `Arc<SharedState>` (recording core).
///
/// # Worker ID resolution
///
/// `WORKER_ID` TLS is populated lazily on the first `on_thread_unpark` / `on_before_task_poll`
/// call via [`resolve_worker_id`](runtime_context::resolve_worker_id), not in `on_thread_start`.
/// This is intentional: `on_thread_start` fires before `RuntimeMetrics` is available, so we
/// cannot yet call `metrics.worker_thread_id(i)` to determine which worker index we are.
/// By the time any waker calls `current_worker_id()`, at least one unpark or poll has occurred
/// and TLS is guaranteed to be populated.
fn register_hooks(
    builder: &mut tokio::runtime::Builder,
    ctx: &Arc<RuntimeContext>,
    shared: &Arc<SharedState>,
    task_tracking_enabled: bool,
) {
    let c1 = ctx.clone();
    let s1 = shared.clone();
    let c2 = ctx.clone();
    let s2 = shared.clone();
    let c3 = ctx.clone();
    let s3 = shared.clone();
    let c4 = ctx.clone();
    let s4 = shared.clone();

    builder
        .on_thread_park(move || {
            let event = make_worker_park(&c1, &s1);
            s1.record_event(event);
        })
        .on_thread_unpark(move || {
            let event = make_worker_unpark(&c2, &s2);
            s2.record_event(event);
        })
        .on_before_task_poll(move |meta| {
            let task_id = TaskId::from(meta.id());
            let location = meta.spawned_at();
            let event = make_poll_start(&c3, &s3, location, task_id);
            s3.record_event(event);
        })
        .on_after_task_poll(move |_meta| {
            let event = make_poll_end(&c4, &s4);
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
                // NOTE: `tokio::runtime::worker_index()` will always return `None` at this point
                // so we can't utilize that here.
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
}

/// Attach a runtime to an existing telemetry session: register hooks, build
/// the runtime, reserve worker IDs, and push the context.
fn attach_runtime(
    shared: &Arc<SharedState>,
    mut builder: tokio::runtime::Builder,
    runtime_name: Option<String>,
    task_tracking_enabled: bool,
) -> std::io::Result<tokio::runtime::Runtime> {
    let ctx = Arc::new(RuntimeContext::new(runtime_name));
    register_hooks(&mut builder, &ctx, shared, task_tracking_enabled);

    let runtime = builder.build()?;

    // Pre-reserve a contiguous block of worker IDs and set metrics atomically.
    let metrics = runtime.handle().metrics();
    let num_workers = metrics.num_workers() as u64;
    let base = shared
        .next_worker_id
        .fetch_add(num_workers, Ordering::Relaxed);
    ctx.metrics_and_base
        .set((metrics, base))
        .unwrap_or_else(|_| {
            tracing::warn!(
                "metrics_and_base already set for runtime context; ignoring duplicate attach"
            );
        });

    shared.contexts.lock().unwrap().push(ctx);

    Ok(runtime)
}

/// Cheap, cloneable handle for controlling telemetry from anywhere.
///
/// Uses a channel to communicate with the flush thread — no shared mutex.
#[derive(Clone)]
pub struct TelemetryHandle {
    shared: Arc<SharedState>,
    control_tx: std::sync::mpsc::SyncSender<ControlCommand>,
}

impl std::fmt::Debug for TelemetryHandle {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("TelemetryHandle").finish_non_exhaustive()
    }
}

impl TelemetryHandle {
    /// Enable telemetry recording.
    pub fn enable(&self) {
        self.shared.enabled.store(true, Ordering::Relaxed);
    }

    /// Disable telemetry recording.
    pub fn disable(&self) {
        self.shared.enabled.store(false, Ordering::Relaxed);
    }

    /// Get a [`TracedHandle`](crate::traced::TracedHandle) for wrapping futures with wake tracking.
    pub fn traced_handle(&self) -> crate::traced::TracedHandle {
        crate::traced::TracedHandle {
            shared: self.shared.clone(),
        }
    }

    /// Spawn a future wrapped with wake-event tracking.
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

/// Handle for spawning wake-tracked futures on a specific runtime.
///
/// Returned by [`TraceRuntimeCoreBuilder::build`]. Unlike [`TelemetryHandle::spawn`]
/// which uses `tokio::spawn()` (requiring an ambient runtime context), this type
/// targets a specific runtime and works from any thread.
///
/// `Clone` is cheap — both inner handles are `Arc`-based.
#[derive(Clone, Debug)]
pub struct RuntimeTelemetryHandle {
    runtime: tokio::runtime::Handle,
    traced: crate::traced::TracedHandle,
}

impl RuntimeTelemetryHandle {
    /// Spawn a future with wake-event tracking on this handle's runtime.
    #[track_caller]
    pub fn spawn<F>(&self, future: F) -> tokio::task::JoinHandle<F::Output>
    where
        F: std::future::Future + Send + 'static,
        F::Output: Send + 'static,
    {
        let traced = self.traced.clone();
        self.runtime.spawn(async move {
            let task_id = tokio::task::try_id().map(TaskId::from).unwrap_or_default();
            crate::traced::Traced::new(future, traced, task_id).await
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

impl std::fmt::Debug for TelemetryGuard {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("TelemetryGuard").finish_non_exhaustive()
    }
}

impl TelemetryGuard {
    /// Get a cloneable handle for controlling telemetry.
    pub fn handle(&self) -> TelemetryHandle {
        self.handle.clone()
    }

    /// Monotonic start time of the telemetry session in nanoseconds.
    pub fn start_time(&self) -> u64 {
        self.handle.shared.start_time_ns
    }

    /// Enable telemetry recording.
    pub fn enable(&self) {
        self.handle.enable();
    }

    /// Disable telemetry recording.
    pub fn disable(&self) {
        self.handle.disable();
    }

    /// Access the shared state for reuse by additional runtimes.
    pub(crate) fn shared(&self) -> &Arc<SharedState> {
        &self.handle.shared
    }

    /// Attach a tokio runtime to this telemetry session.
    ///
    /// Returns a builder that lets you configure per-runtime settings
    /// (e.g. task tracking) before building the runtime.
    ///
    /// ```rust,no_run
    /// # use dial9_tokio_telemetry::telemetry::{NullWriter, TelemetryCore};
    /// # fn main() -> std::io::Result<()> {
    /// let guard = TelemetryCore::builder()
    ///     .writer(NullWriter)
    ///     .build()?;
    /// guard.enable();
    ///
    /// let mut builder = tokio::runtime::Builder::new_multi_thread();
    /// builder.worker_threads(4).enable_all();
    /// let (runtime, handle) = guard.trace_runtime("main").build(builder)?;
    /// # Ok(())
    /// # }
    /// ```
    pub fn trace_runtime(&self, name: impl Into<String>) -> TraceRuntimeCoreBuilder<'_> {
        TraceRuntimeCoreBuilder {
            guard: self,
            name: name.into(),
            task_tracking: false,
        }
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
    /// background worker to drain (symbolize, compress, upload to S3).
    ///
    /// **Call this after the runtime has been dropped** so that Tokio worker
    /// threads have exited and their thread-local telemetry buffers have been
    /// flushed to the central collector.
    ///
    /// ```rust,no_run
    /// # use dial9_tokio_telemetry::telemetry::{RotatingWriter, TracedRuntime};
    /// # use std::time::Duration;
    /// # fn main() -> std::io::Result<()> {
    /// # let writer = RotatingWriter::new("/tmp/t.bin", 1024, 4096)?;
    /// # let builder = tokio::runtime::Builder::new_multi_thread();
    /// let (runtime, guard) = TracedRuntime::build_and_start(builder, writer)?;
    /// runtime.block_on(async { /* ... */ });
    /// drop(runtime); // worker threads exit, flushing thread-local buffers
    /// guard.graceful_shutdown(Duration::from_secs(5))?;
    /// # Ok(())
    /// # }
    /// ```
    ///
    /// Consumes the guard so `Drop` becomes a no-op.
    pub fn graceful_shutdown(mut self, timeout: Duration) -> Result<(), std::io::Error> {
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
            if let Some(t) = w.thread.take()
                && let Err(e) = t.join()
            {
                tracing::error!(target: "dial9_telemetry", panic = ?e, "worker thread panicked during shutdown");
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
#[derive(Debug)]
pub struct NoTracePath;
/// Marker: a trace path has been set.
#[derive(Debug)]
pub struct HasTracePath;

/// Builder for configuring a traced Tokio runtime.
pub struct TracedRuntimeBuilder<P = NoTracePath> {
    enabled: bool,
    task_tracking_enabled: bool,
    trace_path: Option<PathBuf>,
    runtime_name: Option<String>,
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

impl<P> std::fmt::Debug for TracedRuntimeBuilder<P> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("TracedRuntimeBuilder")
            .finish_non_exhaustive()
    }
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

    /// Enable or disable task spawn/terminate tracking.
    pub fn with_task_tracking(mut self, enabled: bool) -> Self {
        self.task_tracking_enabled = enabled;
        self
    }

    /// Set a human-readable name for this runtime. Used in segment metadata
    /// to map runtime indices to names for the trace viewer.
    pub fn with_runtime_name(mut self, name: impl Into<String>) -> Self {
        self.runtime_name = Some(name.into());
        self
    }

    /// Enable CPU profiling with the given configuration (Linux only).
    #[cfg(feature = "cpu-profiling")]
    pub fn with_cpu_profiling(
        mut self,
        config: crate::telemetry::cpu_profile::CpuProfilingConfig,
    ) -> Self {
        self.cpu_profiling_config = Some(config);
        self
    }

    /// Enable per-worker scheduler event capture (Linux only).
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

    /// Set how often the background worker polls for sealed segments.
    pub fn with_worker_poll_interval(mut self, interval: Duration) -> Self {
        self.worker_poll_interval = Some(interval);
        self
    }

    /// Set a metrics sink for the background worker.
    pub fn with_worker_metrics_sink(mut self, sink: metrique_writer::BoxEntrySink) -> Self {
        self.worker_metrics_sink = Some(sink);
        self
    }

    /// Attach a new runtime to an existing telemetry session.
    ///
    /// This reuses the `SharedState`, flush thread, writer, and CPU profiler
    /// from the original `TelemetryGuard`. Only the tokio callbacks are
    /// registered on the new builder. The new runtime's workers get a unique
    /// runtime index so their `WorkerId`s don't collide with existing runtimes.
    pub fn build_and_attach_to_telemetry(
        self,
        builder: tokio::runtime::Builder,
        guard: &TelemetryGuard,
    ) -> std::io::Result<tokio::runtime::Runtime> {
        attach_runtime(
            guard.shared(),
            builder,
            self.runtime_name,
            self.task_tracking_enabled,
        )
    }

    fn into_state<Q>(self) -> TracedRuntimeBuilder<Q> {
        TracedRuntimeBuilder {
            enabled: self.enabled,
            task_tracking_enabled: self.task_tracking_enabled,
            trace_path: self.trace_path,
            runtime_name: self.runtime_name,
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
        builder: tokio::runtime::Builder,
        writer: Box<dyn TraceWriter>,
    ) -> std::io::Result<(tokio::runtime::Runtime, TelemetryGuard)> {
        if !self.enabled {
            return TracedRuntime::build_disabled(builder);
        }

        let core_builder = TelemetryCore::builder()
            .writer(writer)
            .maybe_trace_path(self.trace_path)
            .maybe_worker_poll_interval(self.worker_poll_interval)
            .maybe_worker_metrics_sink(self.worker_metrics_sink);

        #[cfg(feature = "cpu-profiling")]
        let core_builder = core_builder
            .maybe_cpu_profiling(self.cpu_profiling_config)
            .maybe_sched_events(self.sched_event_config);

        #[cfg(feature = "worker-s3")]
        let core_builder = core_builder
            .maybe_s3_config(self.s3_config)
            .maybe_s3_client(self.s3_client);

        let guard = core_builder.build()?;
        let runtime = attach_runtime(
            guard.shared(),
            builder,
            self.runtime_name,
            self.task_tracking_enabled,
        )?;
        Ok((runtime, guard))
    }
}

/// Builder for attaching a runtime to an existing telemetry session.
///
/// Created by [`TelemetryGuard::trace_runtime`]. Call [`.build()`](Self::build)
/// with a [`tokio::runtime::Builder`] to install hooks and build the runtime.
#[must_use]
#[derive(Debug)]
pub struct TraceRuntimeCoreBuilder<'a> {
    guard: &'a TelemetryGuard,
    name: String,
    task_tracking: bool,
}

impl<'a> TraceRuntimeCoreBuilder<'a> {
    /// Enable or disable task spawn/terminate tracking for this runtime.
    /// Defaults to `false`.
    pub fn task_tracking(mut self, enabled: bool) -> Self {
        self.task_tracking = enabled;
        self
    }

    /// Install telemetry hooks, build the runtime, and reserve worker IDs.
    ///
    /// Returns the runtime and a [`RuntimeTelemetryHandle`] for spawning
    /// wake-tracked futures via [`RuntimeTelemetryHandle::spawn`].
    pub fn build(
        self,
        builder: tokio::runtime::Builder,
    ) -> std::io::Result<(tokio::runtime::Runtime, RuntimeTelemetryHandle)> {
        let runtime = attach_runtime(
            self.guard.shared(),
            builder,
            Some(self.name),
            self.task_tracking,
        )?;
        let handle = RuntimeTelemetryHandle {
            runtime: runtime.handle().clone(),
            traced: self.guard.handle().traced_handle(),
        };
        Ok((runtime, handle))
    }
}

/// Entry point for creating a telemetry session decoupled from any tokio runtime.
///
/// Use [`TelemetryCore::builder()`] to configure the session, then call
/// [`TelemetryGuard::trace_runtime`] to attach one or more runtimes.
///
/// ```rust,no_run
/// # use dial9_tokio_telemetry::telemetry::{RotatingWriter, TelemetryCore};
/// # fn main() -> std::io::Result<()> {
/// let writer = RotatingWriter::single_file("/tmp/trace.bin")?;
/// let guard = TelemetryCore::builder()
///     .writer(writer)
///     .build()?;
/// guard.enable();
///
/// let mut builder = tokio::runtime::Builder::new_multi_thread();
/// builder.worker_threads(4).enable_all();
/// let (runtime, handle) = guard.trace_runtime("main").build(builder)?;
/// # Ok(())
/// # }
/// ```
#[derive(Debug)]
pub struct TelemetryCore;

#[bon::bon]
impl TelemetryCore {
    /// Build a telemetry session. Recording starts disabled; call
    /// [`TelemetryGuard::enable`] to begin recording.
    #[builder]
    pub fn new(
        /// The trace writer (e.g. [`RotatingWriter`], [`NullWriter`]).
        writer: impl TraceWriter + 'static,
        /// Path for trace output. Enables the background worker when
        /// cpu-profiling or S3 is configured.
        #[builder(into)]
        trace_path: Option<PathBuf>,
        /// Enable CPU profiling (Linux only).
        #[cfg(feature = "cpu-profiling")]
        cpu_profiling: Option<crate::telemetry::cpu_profile::CpuProfilingConfig>,
        /// Enable scheduler event capture (Linux only).
        #[cfg(feature = "cpu-profiling")]
        sched_events: Option<crate::telemetry::cpu_profile::SchedEventConfig>,
        /// S3 upload configuration.
        #[cfg(feature = "worker-s3")]
        s3_config: Option<crate::background_task::s3::S3Config>,
        /// Pre-built S3 client.
        #[cfg(feature = "worker-s3")]
        s3_client: Option<aws_sdk_s3::Client>,
        /// How often the background worker polls for sealed segments.
        worker_poll_interval: Option<Duration>,
        /// Metrics sink for the flush/worker threads.
        worker_metrics_sink: Option<metrique_writer::BoxEntrySink>,
    ) -> std::io::Result<TelemetryGuard> {
        let start_mono_ns = crate::telemetry::events::clock_monotonic_ns();
        let shared = Arc::new(SharedState::new(start_mono_ns));
        #[allow(unused_mut)]
        let mut event_writer = EventWriter::new(Box::new(writer));

        #[cfg(feature = "cpu-profiling")]
        {
            if let Some(ref config) = cpu_profiling {
                match crate::telemetry::cpu_profile::CpuProfiler::start(config.clone()) {
                    Ok(sampler) => event_writer.cpu_profiler = Some(sampler),
                    Err(e) => tracing::warn!("failed to start CPU profiler: {e}"),
                }
            }
            if let Some(sched_cfg) = sched_events {
                match crate::telemetry::cpu_profile::SchedProfiler::new(sched_cfg) {
                    Ok(sched) => *shared.sched_profiler.lock().unwrap() = Some(sched),
                    Err(e) => tracing::warn!("failed to start scheduler event profiler: {e}"),
                }
            }
        }

        // Channel for TelemetryHandle/Guard → flush thread communication.
        let (control_tx, control_rx) = std::sync::mpsc::sync_channel::<ControlCommand>(1);

        let flush_metrics_sink = worker_metrics_sink
            .clone()
            .unwrap_or_else(metrique_writer::sink::DevNullSink::boxed);

        let flush_thread = {
            let shared = shared.clone();
            std::thread::Builder::new()
                .name("dial9-flush".into())
                .spawn(move || {
                    #[cfg(target_os = "linux")]
                    // SAFETY: nice() is a simple syscall with no memory safety
                    // implications. Increasing the nice value (lowering priority)
                    // is always permitted for unprivileged processes.
                    unsafe {
                        let _ = libc::nice(10);
                    }

                    run_flush_loop(control_rx, &shared, &flush_metrics_sink, event_writer);
                })
                .expect("failed to spawn telemetry-flush thread")
        };

        // Auto-construct worker config when we have a trace path and
        // either cpu-profiling or S3 is configured.
        let worker_config = trace_path.and_then(|trace_path| {
            #[allow(unused_mut)]
            let mut needs_worker = false;
            #[allow(unused_mut)]
            let mut symbolize = false;

            #[cfg(feature = "cpu-profiling")]
            if cpu_profiling.is_some() {
                needs_worker = true;
                symbolize = true;
            }

            #[cfg(feature = "worker-s3")]
            if s3_config.is_some() {
                needs_worker = true;
            }

            if !needs_worker {
                return None;
            }

            let poll_interval =
                worker_poll_interval.unwrap_or(crate::background_task::DEFAULT_POLL_INTERVAL);
            let metrics_sink =
                worker_metrics_sink.unwrap_or_else(metrique_writer::sink::DevNullSink::boxed);

            let config = crate::background_task::BackgroundTaskConfig::builder()
                .trace_path(trace_path)
                .poll_interval(poll_interval)
                .symbolize(symbolize)
                .metrics_sink(metrics_sink);

            #[cfg(feature = "worker-s3")]
            let config = config.maybe_s3(s3_config).maybe_client(s3_client);

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

        Ok(TelemetryGuard {
            handle: TelemetryHandle { shared, control_tx },
            flush_thread: Some(flush_thread),
            worker,
        })
    }
}

/// The flush thread main loop. Extracted so `TelemetryCore::builder` stays readable.
fn run_flush_loop(
    control_rx: std::sync::mpsc::Receiver<ControlCommand>,
    shared: &SharedState,
    flush_metrics_sink: &metrique_writer::BoxEntrySink,
    mut event_writer: EventWriter,
) {
    // Drain the flush thread's own TL buffer every ~1s (200 × 5ms)
    // rather than every cycle, so queue samples and CPU events
    // are batched into reasonably-sized segments.
    let mut cycle_count: u64 = 0;
    const SELF_DRAIN_INTERVAL: u64 = 200;
    // Drain all thread-local buffers every ~30s (6000 × 5ms).
    // This ensures idle/silent threads don't hold events across
    // trace file rotations.
    const TL_DRAIN_INTERVAL: u64 = 6000;
    let sample_interval = Duration::from_millis(10);
    let mut last_sample = Instant::now();
    // Snapshot the user-provided segment metadata so we can
    // merge it with runtime→worker entries on each flush cycle.
    let static_metadata = event_writer.segment_metadata().to_vec();

    loop {
        let mut ack_tx = None;
        let mut exit = false;
        // Wait for control commands up to 5ms.
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
            let contexts = shared.contexts.lock().unwrap().clone();
            let total_global_queue: usize = contexts.iter().map(|c| c.global_queue_depth()).sum();
            if !contexts.is_empty() {
                shared.record_queue_sample(total_global_queue);
            }
        }

        // Merge user-provided metadata with runtime→worker mappings
        // so the next rotated segment is fully self-describing.
        let contexts = shared.contexts.lock().unwrap().clone();
        let runtime_entries: Vec<(String, String)> =
            contexts.iter().filter_map(|c| c.metadata_entry()).collect();
        if !runtime_entries.is_empty() {
            let mut merged = static_metadata.clone();
            merged.extend(runtime_entries);
            event_writer.update_segment_metadata(merged);
        }

        cycle_count += 1;
        let drain_self = exit || cycle_count.is_multiple_of(SELF_DRAIN_INTERVAL);
        // Periodically drain all thread-local buffers so idle/silent
        // threads don't hold events across trace file rotations.
        // We bump the epoch one tick early (~5 ms grace period) so
        // busy threads self-flush on their next record_event,
        // and the intrusive drain only locks truly idle buffers.
        // On exit we bump + drain in the same tick since there is
        // no next tick for the grace period.
        if exit || (cycle_count + 1).is_multiple_of(TL_DRAIN_INTERVAL) {
            shared.bump_drain_epoch();
        }
        if exit || cycle_count.is_multiple_of(TL_DRAIN_INTERVAL) {
            let mut tl_drain_timer = Timer::start_now();
            let stats = shared.drain_all_tl_buffers();
            tl_drain_timer.stop();
            let _guard = TlDrainMetrics {
                operation: Operation::TlDrain,
                duration: tl_drain_timer,
                stats,
                last_drain: exit,
            }
            .append_on_drop(flush_metrics_sink.clone());
        }
        let mut flush_timer = Timer::start_now();
        let stats = flush_once(&mut event_writer, shared, drain_self);
        flush_timer.stop();
        // Create the metrics guard up front; mutate on the exit path,
        // then let it drop (which emits the entry).
        let mut flush_guard =
            (stats.event_count > 0 || stats.dropped_batches > 0 || exit).then(|| {
                FlushMetrics {
                    operation: Operation::Flush,
                    stats,
                    flush_duration: flush_timer,
                    last_flush: exit,
                    write_metadata_failed: false,
                    finalize_failed: false,
                }
                .append_on_drop(flush_metrics_sink.clone())
            });
        if exit {
            // Write final metadata before sealing so single-segment
            // traces contain runtime→worker mappings.
            if let Err(e) = event_writer.write_current_segment_metadata() {
                tracing::warn!("failed to write final segment metadata: {e}");
                if let Some(g) = flush_guard.as_mut() {
                    g.write_metadata_failed = true;
                }
            }
            if let Err(e) = event_writer.finalize() {
                tracing::warn!("failed to finalize trace segment: {e}");
                if let Some(g) = flush_guard.as_mut() {
                    g.finalize_failed = true;
                }
            }
        }
        drop(flush_guard);
        if let Some(tx) = ack_tx.take() {
            let _ = tx.send(());
        }
        if exit {
            return;
        }
    }
}

/// Entry point for setting up a traced Tokio runtime.
#[derive(Debug)]
pub struct TracedRuntime;

impl TracedRuntime {
    /// Create a new [`TracedRuntimeBuilder`].
    pub fn builder() -> TracedRuntimeBuilder<NoTracePath> {
        TracedRuntimeBuilder {
            enabled: true,
            task_tracking_enabled: false,
            trace_path: None,
            runtime_name: None,
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
    use crate::telemetry::collector::CentralCollector;
    use std::panic::Location;
    use std::sync::Arc;
    use std::sync::atomic::AtomicU64;

    /// Drain all pending batches from a `CentralCollector` into an `EventWriter`.
    /// Call `buffer::drain_to_collector` first to flush the thread-local buffer.
    fn drain_collector_to_writer(collector: &CentralCollector, ew: &mut EventWriter) {
        while let Some(batch) = collector.next() {
            if batch.event_count > 0 {
                ew.write_encoded_batch(&batch).unwrap();
            }
        }
    }

    /// Writer that captures encoded bytes for test assertions.
    struct CapturingWriter(Arc<std::sync::Mutex<Vec<u8>>>);
    impl crate::telemetry::writer::TraceWriter for CapturingWriter {
        fn write_encoded_batch(
            &mut self,
            batch: &crate::telemetry::collector::Batch,
        ) -> std::io::Result<()> {
            self.0
                .lock()
                .unwrap()
                .extend_from_slice(batch.encoded_bytes());
            Ok(())
        }
        fn flush(&mut self) -> std::io::Result<()> {
            Ok(())
        }
    }
    #[test]
    fn current_thread_runtime_resolves_worker_ids() {
        let data = Arc::new(std::sync::Mutex::new(Vec::<u8>::new()));

        let mut builder = tokio::runtime::Builder::new_current_thread();
        builder.enable_all();

        let (rt, guard) = TracedRuntime::builder()
            .build_and_start_with_writer(builder, CapturingWriter(data.clone()))
            .unwrap();

        rt.block_on(async {
            tokio::spawn(async {
                tokio::task::yield_now().await;
            })
            .await
            .unwrap();
        });

        drop(rt);
        drop(guard);

        let raw = data.lock().unwrap();
        let events = crate::telemetry::format::decode_events(&raw).unwrap();
        let poll_starts: Vec<_> = events
            .iter()
            .filter_map(|e| match e {
                crate::telemetry::events::TelemetryEvent::PollStart { worker_id, .. } => {
                    Some(*worker_id)
                }
                _ => None,
            })
            .collect();
        assert!(!poll_starts.is_empty(), "expected at least one PollStart");
        let unknown: Vec<_> = poll_starts
            .iter()
            .filter(|id| **id == crate::telemetry::format::WorkerId::UNKNOWN)
            .collect();
        assert!(
            unknown.is_empty(),
            "all PollStart events should have a known worker ID, \
             but {}/{} were UNKNOWN",
            unknown.len(),
            poll_starts.len()
        );
    }

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
    #[cfg(feature = "analysis")]
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
        let collector = Arc::new(CentralCollector::new());
        let drain_epoch = AtomicU64::new(0);

        let locations = [
            location_a, location_b, location_a, location_b, location_a, location_b,
        ];
        for (i, loc) in locations.iter().enumerate() {
            let task_id = crate::telemetry::task_metadata::TaskId::from_u32(i as u32);
            buffer::record_event(
                RawEvent::TaskSpawn {
                    timestamp_nanos: (i as u64 + 1) * 1000,
                    task_id,
                    location: loc,
                },
                &collector,
                &drain_epoch,
            );
            buffer::record_event(
                RawEvent::PollStart {
                    timestamp_nanos: (i as u64 + 1) * 1000,
                    worker_id: WorkerId::from(0usize),
                    worker_local_queue_depth: 0,
                    task_id,
                    location: loc,
                },
                &collector,
                &drain_epoch,
            );
            // Drain after each iteration to produce separate small batches
            // that trigger file rotation (max_file_size is 100 bytes).
            buffer::drain_to_collector(&collector);
            drain_collector_to_writer(&collector, &mut ew);
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
            let reader = TraceReader::new(path).unwrap();

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

            let events = &reader.runtime_events;
            total_events += events.len();
        }
        assert_eq!(
            total_events, 6,
            "all PollStart events should be readable across files"
        );
    }

    #[test]
    fn build_and_attach_to_telemetry_attaches_second_runtime() {
        let builder_a = tokio::runtime::Builder::new_multi_thread();
        let (runtime_a, guard) = TracedRuntime::builder()
            .build_and_start_with_writer(builder_a, NullWriter)
            .unwrap();

        let builder_b = tokio::runtime::Builder::new_multi_thread();
        let runtime_b = TracedRuntime::builder()
            .build_and_attach_to_telemetry(builder_b, &guard)
            .unwrap();

        // Both runtimes should work
        runtime_a.block_on(async {
            let r = tokio::spawn(async { 1 }).await.unwrap();
            assert_eq!(r, 1);
        });
        runtime_b.block_on(async {
            let r = tokio::spawn(async { 2 }).await.unwrap();
            assert_eq!(r, 2);
        });
    }

    #[test]
    fn build_and_attach_to_telemetry_produces_unique_worker_ids() {
        use crate::telemetry::format::WorkerId;
        use std::collections::HashSet;

        let data = Arc::new(std::sync::Mutex::new(Vec::<u8>::new()));

        let mut builder_a = tokio::runtime::Builder::new_multi_thread();
        builder_a.worker_threads(2);
        let (runtime_a, guard) = TracedRuntime::builder()
            .with_task_tracking(true)
            .build_and_start_with_writer(builder_a, CapturingWriter(data.clone()))
            .unwrap();

        let mut builder_b = tokio::runtime::Builder::new_multi_thread();
        builder_b.worker_threads(2);
        let runtime_b = TracedRuntime::builder()
            .with_task_tracking(true)
            .build_and_attach_to_telemetry(builder_b, &guard)
            .unwrap();

        // Generate poll events on both runtimes. Spawn many concurrent tasks
        // to ensure work lands on actual worker threads (not just block_on's thread).
        runtime_a.block_on(async {
            let mut handles = Vec::new();
            for _ in 0..50 {
                handles.push(tokio::spawn(async {
                    tokio::task::yield_now().await;
                }));
            }
            for h in handles {
                h.await.unwrap();
            }
        });
        runtime_b.block_on(async {
            let mut handles = Vec::new();
            for _ in 0..50 {
                handles.push(tokio::spawn(async {
                    tokio::task::yield_now().await;
                }));
            }
            for h in handles {
                h.await.unwrap();
            }
        });

        // Drop runtimes, then guard to flush
        drop(runtime_a);
        drop(runtime_b);
        drop(guard);

        let raw = data.lock().unwrap();
        let captured = crate::telemetry::format::decode_events(&raw).unwrap();
        let mut worker_ids: HashSet<u64> = HashSet::new();
        for event in captured.iter() {
            match event {
                crate::telemetry::events::TelemetryEvent::PollStart { worker_id, .. }
                | crate::telemetry::events::TelemetryEvent::PollEnd { worker_id, .. }
                | crate::telemetry::events::TelemetryEvent::WorkerPark { worker_id, .. }
                | crate::telemetry::events::TelemetryEvent::WorkerUnpark { worker_id, .. } => {
                    if *worker_id != WorkerId::UNKNOWN {
                        worker_ids.insert(worker_id.as_u64());
                    }
                }
                _ => {}
            }
        }

        // Runtime A has 2 workers → IDs 0,1. Runtime B → IDs 2,3.
        // We should see at least one ID from each runtime's range.
        let has_runtime_a = worker_ids.iter().any(|&id| id < 2);
        let has_runtime_b = worker_ids.iter().any(|&id| (2..4).contains(&id));
        assert!(
            has_runtime_a && has_runtime_b,
            "expected worker IDs from both runtimes (0..2 and 2..4), got: {worker_ids:?}"
        );
    }

    /// Verify that `build_and_attach_to_telemetry` propagates the second runtime's metadata
    /// (runtime name → worker ID mapping) into the trace file's segment metadata.
    #[test]
    fn build_and_attach_to_telemetry_propagates_second_runtime_metadata() {
        use crate::telemetry::events::TelemetryEvent;

        let dir = tempfile::TempDir::new().unwrap();
        let trace_path = dir.path().join("trace.bin");

        let writer = crate::telemetry::writer::RotatingWriter::builder()
            .base_path(&trace_path)
            .max_file_size(1024 * 1024)
            .max_total_size(10 * 1024 * 1024)
            .build()
            .unwrap();

        let mut builder_a = tokio::runtime::Builder::new_multi_thread();
        builder_a.worker_threads(2);
        let (runtime_a, guard) = TracedRuntime::builder()
            .with_runtime_name("main")
            .with_trace_path(trace_path.to_str().unwrap())
            .build_and_start(builder_a, writer)
            .unwrap();

        let mut builder_b = tokio::runtime::Builder::new_multi_thread();
        builder_b.worker_threads(2);
        let runtime_b = TracedRuntime::builder()
            .with_runtime_name("io")
            .build_and_attach_to_telemetry(builder_b, &guard)
            .unwrap();

        // Run work on both runtimes so workers resolve their identities.
        for rt in [&runtime_a, &runtime_b] {
            rt.block_on(async {
                let mut handles = Vec::new();
                for _ in 0..20 {
                    handles.push(tokio::spawn(async {
                        tokio::task::yield_now().await;
                    }));
                }
                for h in handles {
                    h.await.unwrap();
                }
            });
        }

        // Give the flush thread time to run (it cycles every 5ms and merges
        // runtime metadata into the writer on each cycle).
        std::thread::sleep(std::time::Duration::from_millis(50));

        drop(runtime_a);
        drop(runtime_b);
        let _ = guard.graceful_shutdown(std::time::Duration::from_secs(5));

        // Read all sealed trace files and collect SegmentMetadata entries.
        let mut all_metadata: Vec<Vec<(String, String)>> = Vec::new();
        let mut files: Vec<_> = std::fs::read_dir(dir.path())
            .unwrap()
            .filter_map(|e| e.ok())
            .map(|e| e.path())
            .filter(|p| p.extension().is_some_and(|ext| ext == "bin"))
            .collect();
        files.sort();
        for file in &files {
            let data = std::fs::read(file).unwrap();
            let events = crate::telemetry::format::decode_events(&data).unwrap();
            for event in &events {
                if let TelemetryEvent::SegmentMetadata { entries, .. } = event {
                    all_metadata.push(entries.clone());
                }
            }
        }

        assert!(
            !all_metadata.is_empty(),
            "expected at least one SegmentMetadata event in trace files"
        );

        // At least one segment's metadata should contain both runtime mappings.
        let has_both = all_metadata.iter().any(|entries| {
            let has_main = entries.iter().any(|(k, _)| k == "runtime.main");
            let has_io = entries.iter().any(|(k, _)| k == "runtime.io");
            has_main && has_io
        });
        assert!(
            has_both,
            "expected segment metadata to contain both runtime.main and runtime.io, \
             got: {all_metadata:?}"
        );
    }

    /// Wake events from runtime B's workers must carry global worker IDs (≥ num_workers_a),
    /// not local indices that collide with runtime A's workers.
    #[test]
    fn wake_events_use_global_worker_id_in_multi_runtime() {
        use crate::telemetry::events::TelemetryEvent;

        let data = Arc::new(std::sync::Mutex::new(Vec::<u8>::new()));

        let mut builder_a = tokio::runtime::Builder::new_multi_thread();
        builder_a.worker_threads(2);
        let (runtime_a, guard) = TracedRuntime::builder()
            .with_task_tracking(true)
            .build_and_start_with_writer(builder_a, CapturingWriter(data.clone()))
            .unwrap();

        let mut builder_b = tokio::runtime::Builder::new_multi_thread();
        builder_b.worker_threads(2);
        let runtime_b = TracedRuntime::builder()
            .with_task_tracking(true)
            .build_and_attach_to_telemetry(builder_b, &guard)
            .unwrap();

        // Use handle.spawn on runtime B to get Traced waker wrapping → wake events.
        let handle = guard.handle();
        runtime_b.block_on(async {
            let mut handles = Vec::new();
            for _ in 0..50 {
                handles.push(handle.spawn(async {
                    tokio::task::yield_now().await;
                }));
            }
            for h in handles {
                h.await.unwrap();
            }
        });

        drop(runtime_a);
        drop(runtime_b);
        drop(guard);

        let raw = data.lock().unwrap();
        let captured = crate::telemetry::format::decode_events(&raw).unwrap();
        let wake_workers: Vec<u8> = captured
            .iter()
            .filter_map(|e| match e {
                TelemetryEvent::WakeEvent { target_worker, .. } => Some(*target_worker),
                _ => None,
            })
            .collect();
        assert!(!wake_workers.is_empty(), "expected at least one WakeEvent");

        // Runtime A has workers 0,1. Runtime B has workers 2,3.
        // Wakes issued from runtime B's workers must have target_worker >= 2.
        let has_global_id = wake_workers.iter().any(|&w| w >= 2 && w != 255);
        assert!(
            has_global_id,
            "expected wake events from runtime B to use global worker IDs (>= 2), \
             but got: {wake_workers:?}"
        );
    }

    #[cfg(all(feature = "cpu-profiling", feature = "analysis"))]
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
                    ew.write_raw_event(RawEvent::CpuSample(Box::new(data)))
                        .unwrap();
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

                for ev in &reader.all_events {
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

    #[test]
    fn telemetry_core_builds_guard_without_runtime() {
        let guard = TelemetryCore::builder().writer(NullWriter).build().unwrap();
        assert!(guard.flush_thread.is_some());
        let _ = guard.graceful_shutdown(std::time::Duration::from_secs(1));
    }

    #[test]
    fn telemetry_core_trace_runtime_produces_working_runtime() {
        let guard = TelemetryCore::builder().writer(NullWriter).build().unwrap();
        guard.enable();

        let mut builder = tokio::runtime::Builder::new_multi_thread();
        builder.worker_threads(2).enable_all();
        let (runtime, _handle) = guard.trace_runtime("main").build(builder).unwrap();

        runtime.block_on(async {
            let r = tokio::spawn(async { 42 }).await.unwrap();
            assert_eq!(r, 42);
        });

        drop(runtime);
        let _ = guard.graceful_shutdown(std::time::Duration::from_secs(1));
    }

    #[test]
    fn telemetry_core_task_tracking_produces_task_spawn_events() {
        let data = Arc::new(std::sync::Mutex::new(Vec::<u8>::new()));
        let guard = TelemetryCore::builder()
            .writer(CapturingWriter(data.clone()))
            .build()
            .unwrap();
        guard.enable();

        let mut builder = tokio::runtime::Builder::new_multi_thread();
        builder.worker_threads(2).enable_all();
        let (runtime, _handle) = guard
            .trace_runtime("main")
            .task_tracking(true)
            .build(builder)
            .unwrap();

        runtime.block_on(async {
            tokio::spawn(async { tokio::task::yield_now().await })
                .await
                .unwrap();
        });

        drop(runtime);
        drop(guard);

        let raw = data.lock().unwrap();
        let events = crate::telemetry::format::decode_events(&raw).unwrap();
        let spawn_count = events
            .iter()
            .filter(|e| {
                matches!(
                    e,
                    crate::telemetry::events::TelemetryEvent::TaskSpawn { .. }
                )
            })
            .count();
        assert!(
            spawn_count > 0,
            "expected TaskSpawn events when task_tracking is enabled, got none"
        );
    }

    #[test]
    fn telemetry_core_trace_runtime_multiple_runtimes_unique_worker_ids() {
        use crate::telemetry::format::WorkerId;
        use std::collections::HashSet;

        let data = Arc::new(std::sync::Mutex::new(Vec::<u8>::new()));
        let guard = TelemetryCore::builder()
            .writer(CapturingWriter(data.clone()))
            .build()
            .unwrap();
        guard.enable();

        let mut builder_a = tokio::runtime::Builder::new_multi_thread();
        builder_a.worker_threads(2).enable_all();
        let (runtime_a, _handle_a) = guard
            .trace_runtime("main")
            .task_tracking(true)
            .build(builder_a)
            .unwrap();

        let mut builder_b = tokio::runtime::Builder::new_multi_thread();
        builder_b.worker_threads(2).enable_all();
        let (runtime_b, _handle_b) = guard
            .trace_runtime("io")
            .task_tracking(true)
            .build(builder_b)
            .unwrap();

        for rt in [&runtime_a, &runtime_b] {
            rt.block_on(async {
                let mut handles = Vec::new();
                for _ in 0..50 {
                    handles.push(tokio::spawn(async {
                        tokio::task::yield_now().await;
                    }));
                }
                for h in handles {
                    h.await.unwrap();
                }
            });
        }

        drop(runtime_a);
        drop(runtime_b);
        drop(guard);

        let raw = data.lock().unwrap();
        let captured = crate::telemetry::format::decode_events(&raw).unwrap();
        let mut worker_ids: HashSet<u64> = HashSet::new();
        for event in &captured {
            if let crate::telemetry::events::TelemetryEvent::PollStart { worker_id, .. } = event
                && *worker_id != WorkerId::UNKNOWN
            {
                worker_ids.insert(worker_id.as_u64());
            }
        }

        let has_runtime_a = worker_ids.iter().any(|&id| id < 2);
        let has_runtime_b = worker_ids.iter().any(|&id| (2..4).contains(&id));
        assert!(
            has_runtime_a && has_runtime_b,
            "expected worker IDs from both runtimes, got: {worker_ids:?}"
        );
    }

    #[test]
    fn trace_runtime_build_returns_telemetry_handle() {
        let data = Arc::new(std::sync::Mutex::new(Vec::<u8>::new()));
        let guard = TelemetryCore::builder()
            .writer(CapturingWriter(data.clone()))
            .build()
            .unwrap();
        guard.enable();

        let mut builder = tokio::runtime::Builder::new_multi_thread();
        builder.worker_threads(2).enable_all();
        let (runtime, handle) = guard.trace_runtime("main").build(builder).unwrap();

        runtime.block_on(async {
            // handle.spawn wraps the future with wake tracking;
            // yield_now triggers a wake so we can verify it's recorded.
            let result = handle
                .spawn(async {
                    tokio::task::yield_now().await;
                    42
                })
                .await
                .unwrap();
            assert_eq!(result, 42);
        });

        // Drain thread-local buffers before shutdown.
        crate::telemetry::buffer::drain_to_collector(
            &guard.handle().traced_handle().shared.collector,
        );

        drop(runtime);
        drop(guard);

        // Verify wake events were recorded (handle.spawn wraps with Traced)
        let raw = data.lock().unwrap();
        let events = crate::telemetry::format::decode_events(&raw).unwrap();
        let wake_count = events
            .iter()
            .filter(|e| {
                matches!(
                    e,
                    crate::telemetry::events::TelemetryEvent::WakeEvent { .. }
                )
            })
            .count();
        assert!(
            wake_count > 0,
            "expected WakeEvent from handle.spawn(), got none"
        );
    }

    /// The handle returned by `trace_runtime().build()` must spawn on the
    /// correct runtime even when called from outside any runtime context.
    #[test]
    fn trace_runtime_handle_spawns_on_correct_runtime_from_outside() {
        let guard = TelemetryCore::builder().writer(NullWriter).build().unwrap();
        guard.enable();

        let mut builder_a = tokio::runtime::Builder::new_multi_thread();
        builder_a.worker_threads(1).enable_all().thread_name("rt-a");
        let (rt_a, handle_a) = guard.trace_runtime("a").build(builder_a).unwrap();

        let mut builder_b = tokio::runtime::Builder::new_multi_thread();
        builder_b.worker_threads(1).enable_all().thread_name("rt-b");
        let (rt_b, handle_b) = guard.trace_runtime("b").build(builder_b).unwrap();

        // Spawn from outside any runtime context — should target the correct runtime.
        let join_a = handle_a.spawn(async {
            tokio::task::yield_now().await;
            std::thread::current().name().unwrap_or("?").to_string()
        });
        let join_b = handle_b.spawn(async {
            tokio::task::yield_now().await;
            std::thread::current().name().unwrap_or("?").to_string()
        });

        let name_a = rt_a.block_on(join_a).unwrap();
        let name_b = rt_b.block_on(join_b).unwrap();

        assert!(
            name_a.starts_with("rt-a"),
            "expected task to run on rt-a, got: {name_a}"
        );
        assert!(
            name_b.starts_with("rt-b"),
            "expected task to run on rt-b, got: {name_b}"
        );

        drop(rt_a);
        drop(rt_b);
        let _ = guard.graceful_shutdown(std::time::Duration::from_secs(1));
    }
}
