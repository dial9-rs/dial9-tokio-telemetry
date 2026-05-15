mod event_writer;
mod runtime_context;
mod shared_state;
#[cfg(feature = "cpu-profiling")]
pub(crate) mod source;

pub(crate) use runtime_context::RuntimeContext;
pub use runtime_context::current_worker_id;
#[cfg(feature = "taskdump")]
pub(crate) use runtime_context::poll_start_ts_or_now;
pub(crate) use shared_state::SharedState;

use event_writer::EventWriter;
use runtime_context::{make_poll_end, make_poll_start, make_worker_park, make_worker_unpark};

use crate::metrics::{FlushMetrics, Operation, TlDrainMetrics};
use crate::primitives::sync::Arc;
use crate::primitives::sync::atomic::Ordering;
use crate::rate_limit::rate_limited;
use crate::telemetry::buffer;
use crate::telemetry::format::TaskTerminateEvent;
use crate::telemetry::task_metadata::TaskId;
use crate::telemetry::writer::{RotatingWriter, TraceWriter};
use metrique::timers::Timer;
use metrique::unit::Microsecond;
use metrique::unit_of_work::metrics;
use metrique_timesource::time_source;
use std::cell::Cell;
use std::cell::RefCell;
use std::path::PathBuf;
use std::time::Duration;

crate::primitives::thread_local! {
    /// Per-thread [`TelemetryHandle`], populated in `on_thread_start` and
    /// cleared in `on_thread_stop`. Enables [`TelemetryHandle::current`].
    static CURRENT_HANDLE: RefCell<Option<TelemetryHandle>> = const { RefCell::new(None) };

    /// Nest count for [`InstrumentedSpawnGuard`]. `on_task_spawn` treats
    /// any value `> 0` as an instrumented spawn.
    static INSTRUMENTED_SPAWN: Cell<u32> = const { Cell::new(0) };
}

// ---------------------------------------------------------------------------
// Channel-based control for the flush thread
// ---------------------------------------------------------------------------

/// Commands sent to the flush thread from TelemetryHandle / TelemetryGuard.
pub(crate) enum ControlCommand {
    /// Flush, finalize (seal segment), then exit the thread.
    FinalizeAndStop(crate::primitives::sync::mpsc::SyncSender<()>),
}

/// Tracks the drain coordination state between the flush loop and the writer.
///
/// When the writer reports a drain is due (`should_drain()`), we can't act
/// immediately because thread-local buffers may still hold events that belong
/// in the current segment. Instead we bump the drain epoch (so threads
/// self-flush on their next `record_event`), wait one cycle (~5 ms) for that
/// to propagate, then perform the intrusive drain + flush + notify the writer
/// via `drained()`.
///
/// Without a state machine, the naïve check `if should_drain { schedule drain }`
/// fires every cycle (since we haven't drained yet), forever deferring the
/// actual drain.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum DrainState {
    /// Normal operation — poll `should_drain()` each cycle.
    Idle,
    /// The writer reported drain due and we bumped the drain epoch.
    /// Next cycle: intrusive drain + flush + `drained()`.
    EpochBumped,
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
    let cpu_events_time = std::time::Instant::now();
    #[cfg(feature = "cpu-profiling")]
    {
        if shared.is_enabled() {
            event_writer.flush_sources(shared);
        }
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
        rate_limited!(Duration::from_secs(60), {
            tracing::warn!(
                dropped_batches = dropped,
                "telemetry flush fell behind, dropped batches"
            );
        });
    }

    while let Some(batch) = shared.collector.next() {
        if batch.event_count > 0
            && let Err(e) = event_writer.write_encoded_batch(&batch)
        {
            rate_limited!(Duration::from_secs(60), {
                tracing::warn!("failed to transcode batch: {e}");
            });
            shared.enabled.store(false, Ordering::Relaxed);
            return FlushStats {
                event_count: event_writer.events_written() - events_before,
                dropped_batches: dropped as u64,
                cpu_flush_duration,
            };
        }
    }
    if let Err(e) = event_writer.flush() {
        rate_limited!(Duration::from_secs(60), {
            tracing::warn!("failed to flush trace data: {e}");
        });
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
    control_tx: &crate::primitives::sync::mpsc::SyncSender<ControlCommand>,
    task_tracking_enabled: bool,
) {
    // TODO: these should rely on public APIs instead of utilizing `SharedState`

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
            s1.if_enabled(|buf| {
                let event = make_worker_park(&c1, &s1);
                buf.record_encodable_event(&event);
            });
        })
        .on_thread_unpark(move || {
            s2.if_enabled(|buf| {
                let event = make_worker_unpark(&c2, &s2);
                buf.record_encodable_event(&event);
            });
        })
        .on_before_task_poll(move |meta| {
            s3.if_enabled(|buf| {
                let task_id = TaskId::from(meta.id());
                let location = meta.spawned_at();
                let event = make_poll_start(&c3, &s3, location, task_id);
                buf.record_encodable_event(&event);
            });
        })
        .on_after_task_poll(move |_meta| {
            s4.if_enabled(|buf| {
                let event = make_poll_end(&c4, &s4);
                buf.record_encodable_event(&event);
            });
        });

    if task_tracking_enabled {
        let s5 = shared.clone();
        builder.on_task_spawn(move |meta| {
            s5.if_enabled(|buf| {
                let task_id = TaskId::from(meta.id());
                let location = meta.spawned_at();
                let instrumented = INSTRUMENTED_SPAWN.with(|f| f.get()) > 0;
                let timestamp_ns = crate::telemetry::events::clock_monotonic_ns();
                buf.record_encodable_event(&runtime_context::TaskSpawn {
                    timestamp_ns,
                    task_id,
                    location,
                    instrumented,
                });
            });
        });
        let s6 = shared.clone();
        builder.on_task_terminate(move |meta| {
            s6.if_enabled(|buf| {
                let task_id = TaskId::from(meta.id());
                buf.record_encodable_event(&TaskTerminateEvent {
                    timestamp_ns: crate::telemetry::events::clock_monotonic_ns(),
                    task_id,
                });
            });
        });
    }

    // Unified on_thread_start / on_thread_stop. Tokio only stores one
    // callback per hook, so any feature-gated work must live here rather
    // than registering its own hook.
    let handle_for_tl = TelemetryHandle::enabled(shared.clone(), control_tx.clone());
    #[cfg(feature = "cpu-profiling")]
    let s_start = shared.clone();
    #[cfg(feature = "cpu-profiling")]
    let s_stop = shared.clone();

    builder
        .on_thread_start(move || {
            // Install this thread's TelemetryHandle so user code can call
            // `TelemetryHandle::current()` from anywhere on this thread.
            CURRENT_HANDLE.with(|cell| {
                *cell.borrow_mut() = Some(handle_for_tl.clone());
            });

            #[cfg(feature = "cpu-profiling")]
            {
                // Register as Blocking initially; worker threads will
                // overwrite this to Worker(i) in resolve_worker_id.
                // NOTE: `tokio::runtime::worker_index()` will always return `None` at this point
                // so we can't utilize that here.
                let tid = crate::telemetry::events::current_tid();
                s_start
                    .thread_roles
                    .lock()
                    .unwrap()
                    .insert(tid, crate::telemetry::events::ThreadRole::Blocking);
                // Sched event sampling is deferred to register_tid_if_needed(),
                // which runs only for worker threads on their first poll/park.
                // This avoids opening perf fds for blocking pool threads.

                // Registers the current thread for the CPU-profiling fallback (ctimer).
                // No-op when perf is the active backend (perf uses inherit).
                let _ = dial9_perf_self_profile::register_current_thread();
            }
        })
        .on_thread_stop(move || {
            CURRENT_HANDLE.with(|cell| {
                *cell.borrow_mut() = None;
            });

            #[cfg(feature = "cpu-profiling")]
            {
                let tid = crate::telemetry::events::current_tid();
                s_stop.thread_roles.lock().unwrap().remove(&tid);
                if let Ok(mut sources) = s_stop.sources.lock() {
                    for source in sources.iter_mut() {
                        source.on_thread_stop();
                    }
                }
                dial9_perf_self_profile::unregister_current_thread();
            }
        });
}

/// Attach a runtime to an existing telemetry session: register hooks, build
/// the runtime, reserve worker IDs, and push the context.
fn attach_runtime(
    shared: &Arc<SharedState>,
    mut builder: tokio::runtime::Builder,
    runtime_name: Option<String>,
    control_tx: &crate::primitives::sync::mpsc::SyncSender<ControlCommand>,
    task_tracking_enabled: bool,
) -> std::io::Result<tokio::runtime::Runtime> {
    let ctx = Arc::new(RuntimeContext::new(runtime_name));
    register_hooks(
        &mut builder,
        &ctx,
        shared,
        control_tx,
        task_tracking_enabled,
    );

    let runtime = builder.build()?;

    // Install the handle on the calling thread. For current_thread runtimes,
    // this thread IS the worker (block_on runs here), so the tracing layer
    // needs CURRENT_HANDLE to be set. Harmless for multi_thread runtimes.
    CURRENT_HANDLE.with(|cell| {
        *cell.borrow_mut() = Some(TelemetryHandle::enabled(shared.clone(), control_tx.clone()));
    });

    // Pre-reserve a contiguous block of worker IDs and set metrics atomically.
    let metrics = runtime.handle().metrics();
    let num_workers = metrics.num_workers() as u64;
    let base = shared
        .next_worker_id
        .fetch_add(num_workers, Ordering::Relaxed);
    ctx.metrics_and_base
        .set((metrics, base))
        .unwrap_or_else(|_| {
            rate_limited!(Duration::from_secs(60), {
                tracing::warn!(
                    "metrics_and_base already set for runtime context; ignoring duplicate attach"
                );
            });
        });

    // Eagerly populate worker_ids so segment metadata is complete from the
    // first flush cycle, rather than waiting for each worker thread to lazily
    // register on its first poll/park event.
    {
        let mut ids = ctx.worker_ids.write().unwrap();
        for i in 0..num_workers {
            ids.insert(i as usize, base + i);
        }
    }

    shared.contexts.lock().unwrap().push(ctx);

    Ok(runtime)
}

/// Cheap, cloneable handle for controlling telemetry from anywhere.
///
/// A handle may be in one of two modes:
///
/// - **Enabled** — backed by a real telemetry session; methods record
///   events, control recording, and wrap spawned futures with wake
///   tracking.
/// - **Disabled** — an inert sentinel returned by
///   [`TelemetryHandle::disabled`] and by [`TelemetryHandle::current`]
///   when called from a thread that is not owned by a dial9 runtime.
///   All methods are no-ops; [`spawn`](Self::spawn) falls back to
///   [`tokio::spawn`] without wake tracking.
///
/// Use [`is_enabled`](Self::is_enabled) to distinguish the two modes.
#[derive(Clone)]
pub struct TelemetryHandle {
    inner: Option<HandleInner>,
}

#[derive(Clone)]
struct HandleInner {
    shared: Arc<SharedState>,
    control_tx: crate::primitives::sync::mpsc::SyncSender<ControlCommand>,
}

impl std::fmt::Debug for TelemetryHandle {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("TelemetryHandle")
            .field("enabled", &self.is_enabled())
            .finish_non_exhaustive()
    }
}

impl TelemetryHandle {
    pub(crate) fn enabled(
        shared: Arc<SharedState>,
        control_tx: crate::primitives::sync::mpsc::SyncSender<ControlCommand>,
    ) -> Self {
        Self {
            inner: Some(HandleInner { shared, control_tx }),
        }
    }

    /// Return an inert handle that is not connected to any telemetry
    /// session. All methods are no-ops; [`spawn`](Self::spawn) falls
    /// back to [`tokio::spawn`] without wake tracking.
    pub fn disabled() -> Self {
        Self { inner: None }
    }

    /// Whether this handle is connected to a live telemetry session.
    ///
    /// Returns `false` for handles obtained via
    /// [`TelemetryHandle::disabled`], and for handles returned by
    /// [`TelemetryHandle::current`] when called from a thread that is
    /// not owned by a dial9 runtime.
    pub fn is_enabled(&self) -> bool {
        self.inner.is_some()
    }

    pub(crate) fn shared(&self) -> Option<&Arc<SharedState>> {
        self.inner.as_ref().map(|i| &i.shared)
    }

    pub(crate) fn control_tx(
        &self,
    ) -> Option<&crate::primitives::sync::mpsc::SyncSender<ControlCommand>> {
        self.inner.as_ref().map(|i| &i.control_tx)
    }

    /// Return the [`TelemetryHandle`] for the current thread.
    ///
    /// On threads owned by a dial9 runtime (workers and blocking
    /// threads — installed via the runtime's `on_thread_start` hook,
    /// cleared on `on_thread_stop`) this returns the live handle for
    /// that runtime.
    ///
    /// On any other thread (including the caller of
    /// `runtime.block_on(...)` on a `current_thread` runtime, threads
    /// outside any tokio context, and threads owned by a runtime built
    /// with telemetry disabled) this returns an inert handle whose
    /// methods are all no-ops — see [`TelemetryHandle::disabled`].
    ///
    /// Use [`is_enabled`](Self::is_enabled) when you need to branch on
    /// whether telemetry is actually live on the current thread.
    pub fn current() -> Self {
        CURRENT_HANDLE
            .with(|cell| cell.borrow().clone())
            .unwrap_or_else(Self::disabled)
    }

    /// Return the [`TelemetryHandle`] installed for the current thread,
    /// or `None` if no dial9 runtime has claimed this thread.
    ///
    /// Prefer [`current`](Self::current) instead.
    pub fn try_current() -> Option<Self> {
        CURRENT_HANDLE.with(|cell| cell.borrow().clone())
    }

    /// Enable telemetry recording. No-op on a disabled handle.
    pub fn enable(&self) {
        if let Some(inner) = &self.inner {
            inner.shared.enabled.store(true, Ordering::Relaxed);
        }
    }

    /// Disable telemetry recording. No-op on a disabled handle.
    pub fn disable(&self) {
        if let Some(inner) = &self.inner {
            inner.shared.enabled.store(false, Ordering::Relaxed);
        }
    }

    /// Get a [`TracedHandle`](crate::traced::TracedHandle) for wrapping
    /// futures with wake tracking, or `None` on a disabled handle.
    pub(crate) fn traced_handle(&self) -> Option<crate::traced::TracedHandle> {
        self.inner.as_ref().map(|i| crate::traced::TracedHandle {
            shared: i.shared.clone(),
        })
    }

    /// Record a user-defined [`Encodable`](crate::telemetry::buffer::Encodable) event.
    ///
    /// No-op on a disabled handle or when recording is paused.
    pub(crate) fn record_encodable_event(&self, event: &dyn crate::telemetry::buffer::Encodable) {
        if let Some(inner) = &self.inner {
            inner
                .shared
                .if_enabled(|buf| buf.record_encodable_event(event));
        }
    }

    /// Run a closure with direct access to the thread-local encoder.
    ///
    /// The closure is only invoked if telemetry is enabled.
    /// No-op on a disabled handle or when recording is paused.
    // TODO(GH-XXX): consider making this public as an alternative to record_event
    // for zero-copy dynamic schema encoding
    pub(crate) fn with_encoder(
        &self,
        f: impl FnOnce(&mut crate::telemetry::buffer::ThreadLocalEncoder<'_>),
    ) {
        if let Some(inner) = &self.inner {
            inner.shared.if_enabled(|buf| buf.with_encoder(f));
        }
    }

    /// Spawn a future on the ambient tokio runtime.
    ///
    /// On an enabled handle, the future is wrapped with wake-event
    /// tracking. On a disabled handle, this is a passthrough to
    /// [`tokio::spawn`].
    ///
    /// # Panics
    ///
    /// Panics if called from outside a tokio runtime context (same
    /// as [`tokio::spawn`]).
    #[track_caller]
    pub fn spawn<F>(&self, future: F) -> tokio::task::JoinHandle<F::Output>
    where
        F: std::future::Future + Send + 'static,
        F::Output: Send + 'static,
    {
        match self.traced_handle() {
            Some(traced_handle) => {
                let _guard = InstrumentedSpawnGuard::enter();
                tokio::spawn(crate::telemetry::TracedFuture::new(
                    future,
                    Some(traced_handle),
                ))
            }
            None => tokio::spawn(future),
        }
    }

    /// Spawn an instrumented future through a user-supplied spawn function.
    ///
    /// `spawn_fn` must synchronously perform a real Tokio spawn (or an
    /// equivalent operation) before returning; do not defer the future or run
    /// it with `block_on`. To record the resulting task as instrumented, spawn
    /// on a dial9-traced runtime with task tracking enabled. The closure's
    /// return value is forwarded back to the caller, so you can keep the
    /// [`tokio::task::JoinHandle`], [`tokio::task::AbortHandle`], or whatever
    /// the spawn function returns.
    ///
    /// # Examples
    ///
    /// Spawn into a [`tokio::task::JoinSet`]:
    ///
    /// ```rust,no_run
    /// # use dial9_tokio_telemetry::telemetry::TelemetryHandle;
    /// # use tokio::task::JoinSet;
    /// # async fn work() {}
    /// # async fn demo() {
    /// let handle = TelemetryHandle::current();
    /// let mut set: JoinSet<()> = JoinSet::new();
    /// handle.spawn_with(work(), |f| set.spawn(f));
    /// # }
    /// ```
    ///
    /// Spawn into a [`tokio::task::JoinSet`] on a specific runtime:
    ///
    /// ```rust,no_run
    /// # use dial9_tokio_telemetry::telemetry::TelemetryHandle;
    /// # use tokio::runtime::Runtime;
    /// # use tokio::task::JoinSet;
    /// # async fn work() {}
    /// # fn demo(runtime: &Runtime) {
    /// let handle = TelemetryHandle::current();
    /// let mut set: JoinSet<()> = JoinSet::new();
    /// handle.spawn_with(work(), |f| set.spawn_on(f, runtime.handle()));
    /// # }
    /// ```
    ///
    /// [`TracedFuture<F>`]: crate::telemetry::TracedFuture
    pub fn spawn_with<F, S>(
        &self,
        future: F,
        spawn_fn: impl FnOnce(crate::telemetry::TracedFuture<F>) -> S,
    ) -> S
    where
        F: std::future::Future + Send + 'static,
        F::Output: Send + 'static,
    {
        let traced_handle = self.traced_handle();
        let future = crate::telemetry::TracedFuture::new(future, traced_handle.clone());
        match traced_handle {
            Some(_) => {
                let _guard = InstrumentedSpawnGuard::enter();
                spawn_fn(future)
            }
            None => spawn_fn(future),
        }
    }
}

/// Spawn a traced task on the current tokio runtime.
///
/// Like [`tokio::spawn`], but wraps the future with wake-event tracking
/// when called from a thread owned by a dial9 runtime. On other threads,
/// falls back to plain [`tokio::spawn`].
///
/// Equivalent to [`TelemetryHandle::current().spawn(future)`](TelemetryHandle::spawn).
///
/// # Panics
///
/// Panics if called from outside a tokio runtime context (same
/// as [`tokio::spawn`]).
#[track_caller]
pub fn spawn<F>(future: F) -> tokio::task::JoinHandle<F::Output>
where
    F: std::future::Future + Send + 'static,
    F::Output: Send + 'static,
{
    TelemetryHandle::current().spawn(future)
}

/// RAII guard that increments `INSTRUMENTED_SPAWN` on creation and
/// decrements it on drop, even if the protected closure panics.
struct InstrumentedSpawnGuard;

impl InstrumentedSpawnGuard {
    fn enter() -> Self {
        INSTRUMENTED_SPAWN.with(|c| c.set(c.get().saturating_add(1)));
        Self
    }
}

impl Drop for InstrumentedSpawnGuard {
    fn drop(&mut self) {
        INSTRUMENTED_SPAWN.with(|c| c.set(c.get().saturating_sub(1)));
    }
}

/// Handle for spawning instrumented futures on a specific runtime.
///
/// Returned by [`TraceRuntimeCoreBuilder::build`]. Unlike [`TelemetryHandle::spawn`]
/// which uses `tokio::spawn()` (requiring an ambient runtime context), this type
/// targets a specific runtime and works from any thread.
///
/// `Clone` is cheap — both inner handles are `Arc`-based.
#[derive(Clone, Debug)]
pub struct RuntimeTelemetryHandle {
    runtime: tokio::runtime::Handle,
    traced: Option<crate::traced::TracedHandle>,
}

impl RuntimeTelemetryHandle {
    /// Spawn a future with wake-event tracking on this handle's runtime.
    ///
    /// On a handle obtained from a disabled [`TelemetryGuard`], wake
    /// tracking is skipped and the future is spawned plainly.
    #[track_caller]
    pub fn spawn<F>(&self, future: F) -> tokio::task::JoinHandle<F::Output>
    where
        F: std::future::Future + Send + 'static,
        F::Output: Send + 'static,
    {
        match self.traced.clone() {
            Some(traced_handle) => {
                let _guard = InstrumentedSpawnGuard::enter();
                self.runtime.spawn(crate::telemetry::TracedFuture::new(
                    future,
                    Some(traced_handle),
                ))
            }
            None => self.runtime.spawn(future),
        }
    }

    /// Spawn an instrumented future through a user-supplied spawn function.
    ///
    /// Mirrors [`TelemetryHandle::spawn_with`] for callers that already hold a
    /// [`RuntimeTelemetryHandle`]. `spawn_fn` must synchronously perform a real
    /// Tokio spawn (or an equivalent operation) before returning; do not defer
    /// the future or run it with `block_on`. To record the resulting task as
    /// instrumented, target a dial9-traced runtime with task tracking enabled,
    /// typically via [`tokio::task::JoinSet::spawn_on`] with the appropriate
    /// [`tokio::runtime::Handle`].
    ///
    /// # Examples
    ///
    /// Spawn into a [`tokio::task::JoinSet`] on a specific runtime:
    ///
    /// ```rust,no_run
    /// # use dial9_tokio_telemetry::telemetry::RuntimeTelemetryHandle;
    /// # use tokio::runtime::Runtime;
    /// # use tokio::task::JoinSet;
    /// # async fn work() {}
    /// # fn demo(runtime: &Runtime, handle: RuntimeTelemetryHandle, set: &mut JoinSet<()>) {
    /// handle.spawn_with(work(), |f| set.spawn_on(f, runtime.handle()));
    /// # }
    /// ```
    ///
    /// [`TracedFuture<F>`]: crate::telemetry::TracedFuture
    pub fn spawn_with<F, S>(
        &self,
        future: F,
        spawn_fn: impl FnOnce(crate::telemetry::TracedFuture<F>) -> S,
    ) -> S
    where
        F: std::future::Future + Send + 'static,
        F::Output: Send + 'static,
    {
        let future = crate::telemetry::TracedFuture::new(future, self.traced.clone());
        match self.traced {
            Some(_) => {
                let _guard = InstrumentedSpawnGuard::enter();
                spawn_fn(future)
            }
            None => spawn_fn(future),
        }
    }
}

/// Holds the background worker thread and its stop signal.
pub(crate) struct WorkerHandle {
    shutdown: Option<tokio::sync::oneshot::Sender<Duration>>,
    thread: Option<crate::primitives::thread::JoinHandle<()>>,
}

/// RAII guard returned by [`TracedRuntimeBuilder::build`].
///
/// A guard is always present on a [`TracedRuntime`], regardless of
/// whether telemetry is enabled. When telemetry is disabled (because
/// the user opted out via `enabled(false)` or because a lenient config
/// path downgraded after a build failure), the guard is in an inert
/// mode: all methods are no-ops, [`handle`](Self::handle) returns an
/// inert [`TelemetryHandle`], and [`graceful_shutdown`](Self::graceful_shutdown)
/// is a successful no-op.
///
/// Use [`is_enabled`](Self::is_enabled) to distinguish the two modes.
pub struct TelemetryGuard {
    inner: GuardInner,
}

enum GuardInner {
    Enabled(EnabledGuard),
    Disabled,
}

struct EnabledGuard {
    handle: TelemetryHandle,
    flush_thread: Option<crate::primitives::thread::JoinHandle<()>>,
    worker: Option<WorkerHandle>,
}

impl std::fmt::Debug for TelemetryGuard {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("TelemetryGuard")
            .field("enabled", &self.is_enabled())
            .finish_non_exhaustive()
    }
}

impl TelemetryGuard {
    pub(crate) fn enabled(
        handle: TelemetryHandle,
        flush_thread: Option<crate::primitives::thread::JoinHandle<()>>,
        worker: Option<WorkerHandle>,
    ) -> Self {
        Self {
            inner: GuardInner::Enabled(EnabledGuard {
                handle,
                flush_thread,
                worker,
            }),
        }
    }

    pub(crate) fn disabled() -> Self {
        Self {
            inner: GuardInner::Disabled,
        }
    }

    /// Whether this guard owns a live telemetry session.
    ///
    /// Returns `false` for guards created by `enabled(false)` configs
    /// or by lenient configs that downgraded after a build failure.
    pub fn is_enabled(&self) -> bool {
        matches!(self.inner, GuardInner::Enabled(_))
    }

    /// Get a cloneable handle for controlling telemetry.
    ///
    /// On a disabled guard this returns an inert handle whose methods
    /// are all no-ops — see [`TelemetryHandle::disabled`].
    pub fn handle(&self) -> TelemetryHandle {
        match &self.inner {
            GuardInner::Enabled(eg) => eg.handle.clone(),
            GuardInner::Disabled => TelemetryHandle::disabled(),
        }
    }

    /// Monotonic start time of the telemetry session in nanoseconds, if
    /// telemetry is enabled.
    pub fn start_time(&self) -> Option<u64> {
        self.shared().map(|s| s.start_time_ns)
    }

    /// Enable telemetry recording. No-op on a disabled guard.
    pub fn enable(&self) {
        if let GuardInner::Enabled(eg) = &self.inner {
            eg.handle.enable();
        }
    }

    /// Disable telemetry recording. No-op on a disabled guard.
    pub fn disable(&self) {
        if let GuardInner::Enabled(eg) = &self.inner {
            eg.handle.disable();
        }
    }

    /// Access the shared state for reuse by additional runtimes.
    pub(crate) fn shared(&self) -> Option<&Arc<SharedState>> {
        match &self.inner {
            GuardInner::Enabled(eg) => eg.handle.shared(),
            GuardInner::Disabled => None,
        }
    }

    pub(crate) fn control_tx(
        &self,
    ) -> Option<&crate::primitives::sync::mpsc::SyncSender<ControlCommand>> {
        match &self.inner {
            GuardInner::Enabled(eg) => eg.handle.control_tx(),
            GuardInner::Disabled => None,
        }
    }

    /// Attach a tokio runtime to this telemetry session.
    ///
    /// Returns a builder that lets you configure per-runtime settings
    /// (e.g. task tracking) before building the runtime.
    ///
    /// On a disabled guard the resulting builder produces a plain tokio
    /// runtime with no telemetry hooks installed.
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
    /// picks up any stragglers. No-op when telemetry is disabled.
    fn stop_flush_thread(&mut self) {
        let GuardInner::Enabled(eg) = &mut self.inner else {
            return;
        };
        // Drain the current thread's buffer (e.g. main thread in block_on)
        // which may contain TaskSpawn events that were never flushed.
        if let Some(shared) = eg.handle.shared() {
            buffer::drain_to_collector(&shared.collector);
        }

        // Tell the flush thread to do a final flush + finalize, then exit.
        let (ack_tx, ack_rx) = crate::primitives::sync::mpsc::sync_channel(0);
        if let Some(tx) = eg.handle.control_tx()
            && tx.send(ControlCommand::FinalizeAndStop(ack_tx)).is_ok()
        {
            let _ = ack_rx.recv();
        }
        if let Some(t) = eg.flush_thread.take() {
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
    /// On a disabled guard this is a successful no-op — there is no
    /// flush thread or background worker to drain.
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

        // 1. Stop flush thread (flushes + finalizes the last segment).
        // No-op when disabled.
        self.stop_flush_thread();
        tracing::debug!(target: "dial9_telemetry", "flush thread joined, segment sealed");

        // 2. Signal worker to drain with the given timeout and wait
        if let GuardInner::Enabled(eg) = &mut self.inner
            && let Some(ref mut w) = eg.worker
        {
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
        // 1. Stop the flush thread (flushes + finalizes). No-op when disabled.
        self.stop_flush_thread();

        // 2. Hard shutdown: drop the sender without sending — worker sees
        // RecvError and exits without draining. No need to join the thread.
        // For graceful drain, use graceful_shutdown() instead.
        if let GuardInner::Enabled(eg) = &mut self.inner
            && let Some(ref mut w) = eg.worker
        {
            w.shutdown.take();
        }
    }
}

/// Marker: no trace path has been set yet.
#[derive(Debug)]
#[non_exhaustive]
pub struct NoTracePath;
/// Marker: a trace path has been set.
#[derive(Debug)]
#[non_exhaustive]
pub struct HasTracePath;

/// Marker: no pipeline strategy has been chosen yet. From this state the
/// builder can transition to either S3 (via `with_s3_uploader`) or a custom
/// pipeline (via `with_custom_pipeline`).
#[derive(Debug)]
#[non_exhaustive]
pub struct PipelineUnset;

/// Marker: the S3 preset has been selected. `with_s3_client` is available
/// to bind a pre-built client; `with_custom_pipeline` is not in scope.
#[derive(Debug)]
#[non_exhaustive]
pub struct PipelineS3;

/// Marker: a custom pipeline has been configured. No further pipeline
/// methods are available.
#[derive(Debug)]
#[non_exhaustive]
pub struct PipelineCustom;

enum PipelineConfig {
    Unset,
    #[cfg(feature = "worker-s3")]
    S3(crate::background_task::S3PipelineUploader),
    Custom(Vec<Box<dyn crate::background_task::SegmentProcessor>>),
}

/// Builder for configuring a traced Tokio runtime.
pub struct TracedRuntimeBuilder<P = NoTracePath, M = PipelineUnset> {
    enabled: bool,
    task_tracking_enabled: bool,
    task_dump_config: Option<crate::telemetry::task_dump_config::TaskDumpConfig>,
    trace_path: Option<PathBuf>,
    runtime_name: Option<String>,
    #[cfg(feature = "cpu-profiling")]
    cpu_profiling_config: Option<crate::telemetry::cpu_profile::CpuProfilingConfig>,
    #[cfg(feature = "cpu-profiling")]
    sched_event_config: Option<crate::telemetry::cpu_profile::SchedEventConfig>,
    pipeline: PipelineConfig,
    /// Static segment metadata to inject into every rotated segment's
    /// header. The S3 preset populates this from `S3Config::as_metadata`
    /// so traces stay self-describing.
    segment_metadata: Vec<(String, String)>,
    worker_poll_interval: Option<Duration>,
    worker_metrics_sink: Option<metrique_writer::BoxEntrySink>,
    _marker: std::marker::PhantomData<(P, M)>,
}

impl<P, M> std::fmt::Debug for TracedRuntimeBuilder<P, M> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("TracedRuntimeBuilder")
            .finish_non_exhaustive()
    }
}

// Methods available regardless of trace-path or pipeline state.
impl<P, M> TracedRuntimeBuilder<P, M> {
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

    /// Capture async backtraces at yield points for tasks that stay idle
    /// longer than the configured threshold.
    ///
    /// Requires the `taskdump` crate feature to actually record events
    pub fn with_task_dumps(
        mut self,
        config: crate::telemetry::task_dump_config::TaskDumpConfig,
    ) -> Self {
        if cfg!(not(feature = "taskdump")) {
            tracing::warn!(
                "taskdumps enabled but `taskdump` feature was not. No task dumps will be captured."
            )
        }
        self.task_dump_config = Some(config);
        self
    }

    /// Set a human-readable name for this runtime. Used in segment metadata
    /// to map runtime indices to names for the trace viewer.
    pub fn with_runtime_name(mut self, name: impl Into<String>) -> Self {
        self.runtime_name = Some(name.into());
        self
    }

    /// Set static metadata embedded as a `SegmentMetadata` event in every
    /// sealed segment file. Read back during analysis and attached to every
    /// Span.
    ///
    /// [`with_s3_uploader`](Self::with_s3_uploader) injects bucket /
    /// service_name / instance_path / boot_id automatically; call this
    /// method when using [`with_custom_pipeline`](Self::with_custom_pipeline)
    /// (or no pipeline) and you still want those entries — or when you want
    /// to override the preset's defaults.
    ///
    /// Repeated calls **replace** the metadata, matching how
    /// `with_s3_uploader` overwrites on a second call. The last call wins,
    /// so `with_segment_metadata` placed *after* `with_s3_uploader`
    /// overrides the preset's injection.
    pub fn with_segment_metadata(mut self, entries: Vec<(String, String)>) -> Self {
        self.segment_metadata = entries;
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
        mut builder: tokio::runtime::Builder,
        guard: &TelemetryGuard,
    ) -> std::io::Result<tokio::runtime::Runtime> {
        let (Some(shared), Some(control_tx)) = (guard.shared(), guard.control_tx()) else {
            // Disabled guard: produce a plain tokio runtime with no
            // telemetry hooks so attaching still works gracefully.
            return builder.build();
        };
        attach_runtime(
            shared,
            builder,
            self.runtime_name,
            control_tx,
            self.task_tracking_enabled,
        )
    }

    fn into_state<Q, N>(self) -> TracedRuntimeBuilder<Q, N> {
        TracedRuntimeBuilder {
            enabled: self.enabled,
            task_tracking_enabled: self.task_tracking_enabled,
            task_dump_config: self.task_dump_config,
            trace_path: self.trace_path,
            runtime_name: self.runtime_name,
            #[cfg(feature = "cpu-profiling")]
            cpu_profiling_config: self.cpu_profiling_config,
            #[cfg(feature = "cpu-profiling")]
            sched_event_config: self.sched_event_config,
            pipeline: self.pipeline,
            segment_metadata: self.segment_metadata,
            worker_poll_interval: self.worker_poll_interval,
            worker_metrics_sink: self.worker_metrics_sink,
            _marker: std::marker::PhantomData,
        }
    }
}

// Pipeline-strategy entry points: only available before a strategy has
// been chosen, so the user picks S3 OR a custom pipeline, not both.
impl<P> TracedRuntimeBuilder<P, PipelineUnset> {
    /// Configure the S3 upload preset for sealed trace segments.
    ///
    /// The resulting pipeline is `[Gzip, S3]` (with `[Symbolize, ...]`
    /// prepended when CPU profiling is enabled). After this call, only
    /// [`with_s3_client`](TracedRuntimeBuilder::with_s3_client) and a
    /// repeated [`with_s3_uploader`](TracedRuntimeBuilder::with_s3_uploader)
    /// override are available — `with_custom_pipeline` is no longer in scope.
    #[cfg(feature = "worker-s3")]
    pub fn with_s3_uploader(
        mut self,
        config: crate::background_task::s3::S3Config,
    ) -> TracedRuntimeBuilder<P, PipelineS3> {
        self.segment_metadata = config
            .as_metadata()
            .map(|(k, v)| (k.to_string(), v.to_string()))
            .collect();
        self.pipeline = PipelineConfig::S3(crate::background_task::S3PipelineUploader::new(
            config, None,
        ));
        self.into_state()
    }

    /// Configure a fully custom processor pipeline. The closure receives a
    /// [`PipelineBuilder`](crate::background_task::PipelineBuilder); chain
    /// methods like `.gzip()`, `.write_back()`, `.s3(cfg)` for built-ins
    /// and `.pipe(processor)` for user-supplied processors.
    ///
    /// Mutually exclusive with [`with_s3_uploader`](Self::with_s3_uploader).
    ///
    /// This is the "full control" path: the resulting pipeline is exactly
    /// what the closure builds, with nothing prepended or appended. In
    /// particular, unlike the S3 preset, this path does **not**:
    /// - auto-populate writer-side segment metadata — call
    ///   [`with_segment_metadata`](Self::with_segment_metadata) if you want
    ///   identity entries (service, host, etc.) embedded in trace files.
    /// - auto-prepend the `Symbolize` step when CPU profiling is enabled.
    ///   Chain
    ///   [`.symbolize()`](crate::background_task::PipelineBuilder::symbolize)
    ///   first if you want symbolized stack frames.
    pub fn with_custom_pipeline<F>(mut self, build: F) -> TracedRuntimeBuilder<P, PipelineCustom>
    where
        F: FnOnce(
            crate::background_task::PipelineBuilder,
        ) -> crate::background_task::PipelineBuilder,
    {
        let pipeline = build(crate::background_task::PipelineBuilder::new());
        self.pipeline = PipelineConfig::Custom(pipeline.into_processors());
        self.into_state()
    }
}

// S3 mode — once the S3 preset is chosen, only S3-specific tweaks remain.
#[cfg(feature = "worker-s3")]
impl<P> TracedRuntimeBuilder<P, PipelineS3> {
    /// Provide a pre-built S3 client (for custom credentials or endpoints).
    /// Replaces any client previously bound to the configured S3 uploader.
    pub fn with_s3_client(mut self, client: aws_sdk_s3::Client) -> Self {
        if let PipelineConfig::S3(ref mut uploader) = self.pipeline {
            uploader.set_client(client);
        }
        self
    }

    /// Replace the configured S3 uploader. A client previously bound via
    /// [`with_s3_client`](Self::with_s3_client) is carried over to the new
    /// uploader so that call order between the two is irrelevant.
    pub fn with_s3_uploader(mut self, config: crate::background_task::s3::S3Config) -> Self {
        self.segment_metadata = config
            .as_metadata()
            .map(|(k, v)| (k.to_string(), v.to_string()))
            .collect();
        let carried = match &mut self.pipeline {
            PipelineConfig::S3(uploader) => uploader.take_client(),
            _ => None,
        };
        self.pipeline = PipelineConfig::S3(crate::background_task::S3PipelineUploader::new(
            config, carried,
        ));
        self
    }
}

impl<M> TracedRuntimeBuilder<NoTracePath, M> {
    /// Set the trace output path. This transitions the builder to
    /// `HasTracePath`, enabling `build()` and `build_and_start()`.
    pub fn with_trace_path(
        mut self,
        path: impl Into<PathBuf>,
    ) -> TracedRuntimeBuilder<HasTracePath, M> {
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
        self.into_state::<HasTracePath, M>()
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

impl<M> TracedRuntimeBuilder<HasTracePath, M> {
    /// Set the trace output path (no-op, already set).
    pub fn with_trace_path(mut self, path: impl Into<PathBuf>) -> Self {
        self.trace_path = Some(path.into());
        self
    }

    /// Build the traced runtime with a `RotatingWriter`.
    ///
    /// The background worker is auto-spawned when cpu-profiling or any
    /// pipeline strategy is configured. Recording starts disabled; call
    /// [`TelemetryGuard::enable`] to begin, or use
    /// [`build_and_start`](Self::build_and_start).
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
    /// still spawned if cpu-profiling or any pipeline strategy is configured
    /// and `trace_path` is set.
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

        let processors = assemble_processors(
            #[cfg(feature = "cpu-profiling")]
            self.cpu_profiling_config.is_some(),
            self.pipeline,
        );

        let core_builder = TelemetryCore::builder()
            .writer(writer)
            .maybe_trace_path(self.trace_path)
            .maybe_task_dump_config(self.task_dump_config)
            .maybe_worker_poll_interval(self.worker_poll_interval)
            .maybe_worker_metrics_sink(self.worker_metrics_sink)
            .processors(processors)
            .segment_metadata(self.segment_metadata);

        #[cfg(feature = "cpu-profiling")]
        let core_builder = core_builder
            .maybe_cpu_profiling(self.cpu_profiling_config)
            .maybe_sched_events(self.sched_event_config);

        let guard = core_builder.build()?;
        let control_tx = guard
            .control_tx()
            .expect("TelemetryCore::builder().build() always returns an enabled guard")
            .clone();
        let shared = guard
            .shared()
            .expect("TelemetryCore::builder().build() always returns an enabled guard");
        let runtime = attach_runtime(
            shared,
            builder,
            self.runtime_name,
            &control_tx,
            self.task_tracking_enabled,
        )?;
        Ok((runtime, guard))
    }
}

/// Build the final processor pipeline.
///
/// `Symbolize` is auto-prepended for the built-in presets (`Unset`, `S3`)
/// when CPU profiling is enabled. The `Custom` path is "full control" — the
/// user's processor list is passed through verbatim, and they're expected to
/// chain [`PipelineBuilder::symbolize`](crate::background_task::PipelineBuilder::symbolize)
/// themselves if they want symbolization.
///
/// Behaviour matrix:
///
/// | strategy | CPU profiling on               | CPU profiling off |
/// |----------|--------------------------------|-------------------|
/// | Unset    | `[Symbolize, Gzip, WriteBack]` | (worker skipped)  |
/// | S3       | `[Symbolize, Gzip, S3]`        | `[Gzip, S3]`      |
/// | Custom   | `[...user]`                    | `[...user]`       |
fn assemble_processors(
    #[cfg(feature = "cpu-profiling")] cpu_profiling_enabled: bool,
    pipeline: PipelineConfig,
) -> Vec<Box<dyn crate::background_task::SegmentProcessor>> {
    #[cfg(not(feature = "cpu-profiling"))]
    let cpu_profiling_enabled = false;

    if matches!(pipeline, PipelineConfig::Unset) && !cpu_profiling_enabled {
        return Vec::new();
    }

    let mut processors: Vec<Box<dyn crate::background_task::SegmentProcessor>> = Vec::new();
    match pipeline {
        PipelineConfig::Unset => {
            #[cfg(feature = "cpu-profiling")]
            if cpu_profiling_enabled {
                processors.push(Box::new(crate::background_task::SymbolizeProcessor));
            }
            processors.push(Box::new(crate::background_task::GzipCompressor));
            processors.push(Box::new(crate::background_task::WriteBackProcessor));
        }
        #[cfg(feature = "worker-s3")]
        PipelineConfig::S3(uploader) => {
            #[cfg(feature = "cpu-profiling")]
            if cpu_profiling_enabled {
                processors.push(Box::new(crate::background_task::SymbolizeProcessor));
            }
            processors.push(Box::new(crate::background_task::GzipCompressor));
            processors.push(Box::new(uploader));
        }
        PipelineConfig::Custom(user) => {
            processors.extend(user);
        }
    }
    processors
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
    /// instrumented futures via [`RuntimeTelemetryHandle::spawn`].
    pub fn build(
        self,
        mut builder: tokio::runtime::Builder,
    ) -> std::io::Result<(tokio::runtime::Runtime, RuntimeTelemetryHandle)> {
        let (Some(shared), Some(control_tx), Some(traced)) = (
            self.guard.shared(),
            self.guard.control_tx(),
            self.guard.handle().traced_handle(),
        ) else {
            // Disabled guard: build a plain tokio runtime and return a
            // RuntimeTelemetryHandle that effectively short-circuits to
            // tokio::spawn.
            let runtime = builder.build()?;
            let handle = RuntimeTelemetryHandle {
                runtime: runtime.handle().clone(),
                traced: None,
            };
            return Ok((runtime, handle));
        };
        let runtime = attach_runtime(
            shared,
            builder,
            Some(self.name),
            control_tx,
            self.task_tracking,
        )?;
        let handle = RuntimeTelemetryHandle {
            runtime: runtime.handle().clone(),
            traced: Some(traced),
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
    #[builder(state_mod = telemetry_core_builder)]
    pub fn new(
        /// The pipeline of [`SegmentProcessor`](crate::background_task::SegmentProcessor)s
        /// to run on each sealed segment. When empty the background worker
        /// is not spawned.
        #[builder(field)]
        processors: Vec<Box<dyn crate::background_task::SegmentProcessor>>,
        /// Static segment metadata injected into every rotated segment's
        /// header. Empty by default; the S3 preset populates it from the
        /// configured `S3Config` so traces stay self-describing.
        #[builder(field)]
        segment_metadata: Vec<(String, String)>,
        /// S3 upload configuration.
        #[cfg(feature = "worker-s3")]
        #[builder(field)]
        s3_config: Option<crate::background_task::s3::S3Config>,
        /// Pre-built S3 client.
        #[cfg(feature = "worker-s3")]
        #[builder(field)]
        s3_client: Option<aws_sdk_s3::Client>,
        /// The trace writer (e.g. [`RotatingWriter`], [`NullWriter`]).
        writer: impl TraceWriter + 'static,
        /// Path for trace output. Enables the background worker when any
        /// segment processors are configured.
        #[builder(into)]
        trace_path: Option<PathBuf>,
        /// Capture async backtraces at yield points. Requires the `taskdump`
        /// crate feature to actually record events.
        task_dump_config: Option<crate::telemetry::task_dump_config::TaskDumpConfig>,
        /// Enable CPU profiling (Linux only).
        #[cfg(feature = "cpu-profiling")]
        cpu_profiling: Option<crate::telemetry::cpu_profile::CpuProfilingConfig>,
        /// Enable scheduler event capture (Linux only).
        #[cfg(feature = "cpu-profiling")]
        sched_events: Option<crate::telemetry::cpu_profile::SchedEventConfig>,
        /// How often the background worker polls for sealed segments.
        worker_poll_interval: Option<Duration>,
        /// Metrics sink for the flush/worker threads.
        worker_metrics_sink: Option<metrique_writer::BoxEntrySink>,
    ) -> std::io::Result<TelemetryGuard> {
        let start_mono_ns = crate::telemetry::events::clock_monotonic_ns();
        let rng_seed = task_dump_config.as_ref().and_then(|cfg| cfg.rng_seed());
        let shared = Arc::new(SharedState::new(start_mono_ns, rng_seed));
        if let Some(cfg) = task_dump_config.as_ref() {
            shared.task_dumps_enabled.store(true, Ordering::Relaxed);
            shared
                .task_dump_idle_threshold_ns
                .store(cfg.idle_threshold().as_nanos() as u64, Ordering::Relaxed);
        }

        // Determine the pipeline strategy from the builder fields, then
        // delegate to `assemble_processors` — the single source of truth for
        // which processors are used in each configuration.
        #[allow(unused_mut)]
        let mut segment_metadata = segment_metadata;

        #[allow(unused_variables)]
        let pipeline = if !processors.is_empty() {
            PipelineConfig::Custom(processors)
        } else {
            #[cfg(feature = "worker-s3")]
            if let Some(config) = s3_config {
                if segment_metadata.is_empty() {
                    segment_metadata = config
                        .as_metadata()
                        .map(|(k, v)| (k.to_string(), v.to_string()))
                        .collect();
                }
                PipelineConfig::S3(crate::background_task::S3PipelineUploader::new(
                    config, s3_client,
                ))
            } else {
                PipelineConfig::Unset
            }
            #[cfg(not(feature = "worker-s3"))]
            PipelineConfig::Unset
        };

        let processors = assemble_processors(
            #[cfg(feature = "cpu-profiling")]
            cpu_profiling.is_some(),
            pipeline,
        );

        #[allow(unused_mut)]
        let mut event_writer = EventWriter::new(Box::new(writer));

        if !segment_metadata.is_empty() {
            event_writer.update_segment_metadata(segment_metadata);
        }

        #[cfg(feature = "cpu-profiling")]
        {
            if let Some(ref config) = cpu_profiling {
                match crate::telemetry::cpu_profile::CpuProfiler::start(config.clone()) {
                    Ok(sampler) => shared.sources.lock().unwrap().push(Box::new(sampler)),
                    Err(e) => rate_limited!(Duration::from_secs(60), {
                        tracing::warn!("failed to start CPU profiler: {e}");
                    }),
                }
            }
            if let Some(sched_cfg) = sched_events {
                match crate::telemetry::cpu_profile::SchedProfiler::new(sched_cfg) {
                    Ok(sched) => shared.sources.lock().unwrap().push(Box::new(sched)),
                    Err(e) => rate_limited!(Duration::from_secs(60), {
                        tracing::warn!("failed to start scheduler event profiler: {e}");
                    }),
                }
            }
        }

        // Channel for TelemetryHandle/Guard → flush thread communication.
        let (control_tx, control_rx) =
            crate::primitives::sync::mpsc::sync_channel::<ControlCommand>(1);

        let flush_metrics_sink = worker_metrics_sink
            .clone()
            .unwrap_or_else(metrique_writer::sink::DevNullSink::boxed);

        let flush_thread = {
            let shared = shared.clone();
            crate::primitives::thread::spawn_named("dial9-flush", move || {
                #[cfg(target_os = "linux")]
                // SAFETY: nice() is a simple syscall with no memory safety
                // implications. Increasing the nice value (lowering priority)
                // is always permitted for unprivileged processes.
                unsafe {
                    let _ = libc::nice(10);
                }

                #[cfg(feature = "cpu-profiling")]
                let _ = dial9_perf_self_profile::register_current_thread();
                run_flush_loop(control_rx, &shared, &flush_metrics_sink, event_writer);
                #[cfg(feature = "cpu-profiling")]
                dial9_perf_self_profile::unregister_current_thread();
            })
        };

        // Auto-construct worker config when we have a trace path and
        // at least one processor. When the user supplies no processors
        // there is nothing for the worker to do, so skip spawning it.
        let worker_config = trace_path.and_then(|trace_path| {
            if processors.is_empty() {
                return None;
            }

            let poll_interval =
                worker_poll_interval.unwrap_or(crate::background_task::DEFAULT_POLL_INTERVAL);
            let metrics_sink =
                worker_metrics_sink.unwrap_or_else(metrique_writer::sink::DevNullSink::boxed);

            Some(
                crate::background_task::BackgroundTaskConfig::builder()
                    .trace_path(trace_path)
                    .poll_interval(poll_interval)
                    .processors(processors)
                    .metrics_sink(metrics_sink)
                    .build(),
            )
        });

        #[allow(unused_mut)]
        let mut worker = None;
        if let Some(config) = worker_config {
            let (shutdown_tx, shutdown_rx) = tokio::sync::oneshot::channel();
            let wt = crate::primitives::thread::spawn_named("dial9-worker", move || {
                #[cfg(feature = "cpu-profiling")]
                let _ = dial9_perf_self_profile::register_current_thread();
                crate::background_task::run_background_task(config, shutdown_rx);
                #[cfg(feature = "cpu-profiling")]
                dial9_perf_self_profile::unregister_current_thread();
            });
            worker = Some(WorkerHandle {
                shutdown: Some(shutdown_tx),
                thread: Some(wt),
            });
        }

        Ok(TelemetryGuard::enabled(
            TelemetryHandle::enabled(shared, control_tx),
            Some(flush_thread),
            worker,
        ))
    }
}

// Custom methods on the generated builder.
impl<W: TraceWriter, S: telemetry_core_builder::State> TelemetryCoreBuilder<W, S> {
    /// Configure S3 upload for sealed trace segments.
    #[cfg(feature = "worker-s3")]
    pub fn s3_config(mut self, config: crate::background_task::s3::S3Config) -> Self {
        self.s3_config = Some(config);
        self
    }

    /// Provide a pre-built S3 client (for custom credentials or endpoints).
    #[cfg(feature = "worker-s3")]
    pub fn s3_client(mut self, client: aws_sdk_s3::Client) -> Self {
        self.s3_client = Some(client);
        self
    }

    /// Set the processor pipeline directly.
    pub fn processors(
        mut self,
        processors: Vec<Box<dyn crate::background_task::SegmentProcessor>>,
    ) -> Self {
        self.processors = processors;
        self
    }

    /// Set static segment metadata.
    pub fn segment_metadata(mut self, entries: Vec<(String, String)>) -> Self {
        self.segment_metadata = entries;
        self
    }
}

/// The flush thread main loop. Extracted so `TelemetryCore::builder` stays readable.
fn run_flush_loop(
    control_rx: crate::primitives::sync::mpsc::Receiver<ControlCommand>,
    shared: &SharedState,
    flush_metrics_sink: &metrique_writer::BoxEntrySink,
    mut event_writer: EventWriter,
) {
    // Drain the flush thread's own TL buffer every ~1s (200 × 5ms)
    // rather than every cycle, so queue samples and CPU events
    // are batched into reasonably-sized segments.
    let mut cycle_count: u64 = 0;
    const SELF_DRAIN_INTERVAL: u64 = 200;

    let sample_interval = Duration::from_millis(10);
    let mut last_sample = time_source().instant();
    // Snapshot the user-provided segment metadata so we can
    // merge it with runtime→worker entries on each flush cycle.
    let static_metadata = event_writer.segment_metadata().to_vec();

    let mut drain_state = DrainState::Idle;

    loop {
        let mut ack_tx = None;
        let mut exit = false;
        // Wait for control commands up to 5ms.
        match control_rx.recv_timeout(Duration::from_millis(5)) {
            Ok(ControlCommand::FinalizeAndStop(ack)) => {
                ack_tx = Some(ack);
                exit = true;
            }
            Err(crate::primitives::sync::mpsc::RecvTimeoutError::Disconnected) => {
                // All senders dropped — do a best-effort finalize.
                exit = true;
            }
            Err(crate::primitives::sync::mpsc::RecvTimeoutError::Timeout) => {}
        }

        // When disabled, skip all recording work (queue sampling, metadata
        // merging, drain coordination, flush). The loop still wakes every
        // 5ms to check for control commands and the exit signal.
        if !exit && !shared.is_enabled() {
            continue;
        }

        if last_sample.elapsed() >= sample_interval {
            last_sample = time_source().instant();
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
        // --- Drain coordination state machine ---
        //
        // When the writer reports a drain is due, we can't act immediately
        // because thread-local buffers may still hold events that belong
        // in the current segment.  The two-state machine ensures we:
        //   Idle        → detect should_drain, bump epoch, transition
        //   EpochBumped → intrusive drain + flush + drained(), back to Idle
        //
        // This avoids the bug where re-checking should_drain() every
        // cycle (it stays true until we actually call drained()) would
        // forever reschedule the drain and never reach the drained step.
        let do_drain = match drain_state {
            DrainState::Idle => {
                if !exit && event_writer.should_drain() {
                    shared.bump_drain_epoch();
                    drain_state = DrainState::EpochBumped;
                }
                false
            }
            DrainState::EpochBumped => {
                drain_state = DrainState::Idle;
                true
            }
        };

        // On exit, bump + drain in the same tick since there is no next
        // tick for the grace period.
        if exit {
            shared.bump_drain_epoch();
        }

        // --- Execute intrusive drain when needed ---
        if exit || do_drain {
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

        // Notify the writer that TL buffers have been drained and flushed.
        // The writer may rotate the segment or just advance its drain timer.
        // Skip on exit — finalize() below will seal the final segment.
        if do_drain
            && !exit
            && let Err(e) = event_writer.drained()
        {
            rate_limited!(Duration::from_secs(60), {
                tracing::warn!("failed to complete post-drain action: {e}");
            });
        }

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
                rate_limited!(Duration::from_secs(60), {
                    tracing::warn!("failed to write final segment metadata: {e}");
                });
                if let Some(g) = flush_guard.as_mut() {
                    g.write_metadata_failed = true;
                }
            }
            if let Err(e) = event_writer.finalize() {
                rate_limited!(Duration::from_secs(60), {
                    tracing::warn!("failed to finalize trace segment: {e}");
                });
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

/// A tokio runtime paired with its (optional) dial9 telemetry guard.
///
/// The guard, when present, must outlive the runtime so traces are flushed
/// on drop — keeping both inside one struct enforces that ordering at the
/// type level (fields drop top-to-bottom, so `runtime` drops before `guard`).
///
/// Construct one of two ways:
///
/// - **High-level**: from a [`crate::Dial9Config`] via [`TracedRuntime::new`]
///   (panicking, used by the `#[dial9_tokio_telemetry::main]` macro) or
///   [`TracedRuntime::try_new`] (fallible).
/// - **Low-level**: via [`TracedRuntime::builder`] →
///   [`TracedRuntimeBuilder::build_and_start`] for direct control over the
///   raw [`tokio::runtime::Builder`] and [`crate::telemetry::TraceWriter`].
///   This is the path used by example code, benchmarks, and integration
///   tests that want to wire a [`crate::telemetry::NullWriter`] or other
///   custom writer.
#[derive(Debug)]
pub struct TracedRuntime {
    pub(crate) runtime: tokio::runtime::Runtime,
    pub(crate) guard: TelemetryGuard,
}

impl TracedRuntime {
    /// Create a new [`TracedRuntimeBuilder`].
    pub fn builder() -> TracedRuntimeBuilder<NoTracePath, PipelineUnset> {
        TracedRuntimeBuilder {
            enabled: true,
            task_tracking_enabled: false,
            task_dump_config: None,
            trace_path: None,
            runtime_name: None,
            #[cfg(feature = "cpu-profiling")]
            cpu_profiling_config: None,
            #[cfg(feature = "cpu-profiling")]
            sched_event_config: None,
            pipeline: PipelineConfig::Unset,
            segment_metadata: Vec::new(),
            worker_poll_interval: None,
            worker_metrics_sink: None,
            _marker: std::marker::PhantomData,
        }
    }

    /// Build a plain runtime with no telemetry installed.
    ///
    /// The returned [`TelemetryGuard`] is in its disabled mode — see
    /// [`TelemetryGuard::is_enabled`].
    pub fn build_disabled(
        mut builder: tokio::runtime::Builder,
    ) -> std::io::Result<(tokio::runtime::Runtime, TelemetryGuard)> {
        let runtime = builder.build()?;
        Ok((runtime, TelemetryGuard::disabled()))
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

// ---------------------------------------------------------------------------
// High-level construction: TracedRuntime::new / try_new from Dial9Config
// ---------------------------------------------------------------------------
//
// These are the entry points the `#[dial9_tokio_telemetry::main]` macro and
// hand-written `main` functions reach for. The low-level
// `TracedRuntime::builder()` / `build_and_start()` above stay available for
// callers (benches, integration tests, custom-writer setups) that need to
// drive the [`tokio::runtime::Builder`] directly.

/// Errors produced while constructing a [`TracedRuntime`] from a
/// [`crate::Dial9Config`].
///
/// Writer-transport I/O has already been validated by
/// [`crate::Dial9ConfigBuilder::build`], so the only remaining failure
/// modes here come from the tokio runtime builder and the telemetry
/// background worker startup.
#[derive(Debug)]
#[non_exhaustive]
pub enum TelemetryRuntimeError {
    /// Failure from [`tokio::runtime::Builder::build`].
    TokioRuntimeBuilder(std::io::Error),
    /// Failure from telemetry core setup (traced runtime + background worker).
    TelemetryCore(std::io::Error),
}

impl std::fmt::Display for TelemetryRuntimeError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            TelemetryRuntimeError::TokioRuntimeBuilder(e) => {
                write!(f, "tokio runtime builder: {e}")
            }
            TelemetryRuntimeError::TelemetryCore(e) => write!(f, "telemetry core: {e}"),
        }
    }
}

impl std::error::Error for TelemetryRuntimeError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            TelemetryRuntimeError::TokioRuntimeBuilder(e)
            | TelemetryRuntimeError::TelemetryCore(e) => Some(e),
        }
    }
}

/// Drive a [`crate::current_config::Inner`] to a tokio runtime + guard.
///
/// `Inner::Enabled` already carries a built [`RotatingWriter`], so this
/// only needs to materialize the tokio builder and hand both off to
/// [`TracedRuntimeBuilder::build_and_start`]. `Inner::Disabled`
/// produces a plain tokio runtime paired with a disabled
/// [`TelemetryGuard`].
fn try_assemble_dial9_config(
    inner: crate::current_config::Inner,
) -> Result<(tokio::runtime::Runtime, TelemetryGuard), TelemetryRuntimeError> {
    use crate::current_config::{Inner, materialize_tokio_builder};

    match inner {
        Inner::Enabled {
            writer,
            tokio_configurators,
            runtime_builder,
        } => {
            let tokio_builder = materialize_tokio_builder(&tokio_configurators);
            let (runtime, guard) = runtime_builder
                .build_and_start(tokio_builder, writer)
                .map_err(TelemetryRuntimeError::TelemetryCore)?;
            Ok((runtime, guard))
        }
        Inner::Disabled {
            tokio_configurators,
        } => {
            let runtime = materialize_tokio_builder(&tokio_configurators)
                .build()
                .map_err(TelemetryRuntimeError::TokioRuntimeBuilder)?;
            Ok((runtime, TelemetryGuard::disabled()))
        }
    }
}

impl TracedRuntime {
    /// Build a [`TracedRuntime`] from a config, panicking with the
    /// underlying error on failure. Used by the
    /// `#[dial9_tokio_telemetry::main]` macro.
    ///
    /// Reach for this directly when the macro doesn't fit — e.g. when an
    /// application owns multiple tokio runtimes, when you need to control
    /// runtime lifetime explicitly, or when you want to drive
    /// [`TelemetryGuard::graceful_shutdown`] before the runtime drops.
    ///
    /// Generic over any input that converts into a [`TracedRuntime`]: in
    /// practice that means either the fluent
    /// [`crate::Dial9Config`] (returned by
    /// [`Dial9Config::builder`](crate::Dial9Config::builder)) or the
    /// deprecated positional [`crate::config::Dial9Config`]. The generic
    /// shape is what keeps the macro source-compatible across these
    /// input types.
    ///
    /// # Panics
    ///
    /// Panics if the underlying conversion fails — i.e. if the tokio
    /// runtime cannot be built or the telemetry background worker fails
    /// to start. When constructing from the fluent
    /// [`crate::Dial9Config`], writer-transport I/O has already been
    /// validated by
    /// [`Dial9ConfigBuilder::build`](crate::Dial9ConfigBuilder::build),
    /// so the only remaining failure modes are tokio-builder and
    /// telemetry-core startup I/O.
    ///
    /// For fallible construction, use [`try_new`](Self::try_new).
    ///
    /// ```no_run
    /// use dial9_tokio_telemetry::{Dial9Config, TracedRuntime};
    /// let cfg = Dial9Config::builder()
    ///     .base_path("trace.bin")
    ///     .max_file_size(64 * 1024 * 1024)
    ///     .max_total_size(1024 * 1024 * 1024)
    ///     .build()?;
    /// let rt = TracedRuntime::new(cfg);
    /// rt.block_on(async { /* ... */ });
    /// # Ok::<(), Box<dyn std::error::Error>>(())
    /// ```
    pub fn new<C>(config: C) -> Self
    where
        C: TryInto<TracedRuntime>,
        <C as TryInto<TracedRuntime>>::Error: std::fmt::Display,
    {
        config
            .try_into()
            .unwrap_or_else(|e| panic!("failed to initialize runtime: {e}"))
    }

    /// Fallible counterpart to [`new`](Self::new).
    ///
    /// Returns the conversion error directly: when constructing from
    /// [`crate::Dial9Config`] that's a [`TelemetryRuntimeError`]; when
    /// constructing from the deprecated [`crate::config::Dial9Config`]
    /// it's a [`std::io::Error`]. Use this when you want to handle
    /// runtime construction failure rather than panic.
    ///
    /// ```no_run
    /// use dial9_tokio_telemetry::{Dial9Config, TracedRuntime};
    /// let cfg = Dial9Config::builder()
    ///     .base_path("trace.bin")
    ///     .max_file_size(64 * 1024 * 1024)
    ///     .max_total_size(1024 * 1024 * 1024)
    ///     .build()?;
    /// let rt = TracedRuntime::try_new(cfg)?;
    /// rt.block_on(async { /* ... */ });
    /// # Ok::<(), Box<dyn std::error::Error>>(())
    /// ```
    pub fn try_new<C>(config: C) -> Result<Self, <C as TryInto<TracedRuntime>>::Error>
    where
        C: TryInto<TracedRuntime>,
    {
        config.try_into()
    }

    /// Borrow the underlying tokio runtime.
    pub fn runtime(&self) -> &tokio::runtime::Runtime {
        &self.runtime
    }

    /// Borrow the telemetry guard.
    ///
    /// The guard is always present, regardless of whether telemetry was
    /// installed. Use [`TelemetryGuard::is_enabled`] to distinguish a
    /// live telemetry session from an inert (disabled) guard.
    pub fn guard(&self) -> &TelemetryGuard {
        &self.guard
    }

    /// Run `fut` to completion on the runtime.
    ///
    /// The future is always spawned through the guard's
    /// [`TelemetryHandle`]. On an enabled guard this records poll and
    /// wake events; on a disabled guard the handle's `spawn` falls
    /// through to plain [`tokio::spawn`].
    pub fn block_on<F>(&self, fut: F) -> F::Output
    where
        F: std::future::Future + Send + 'static,
        F::Output: Send + 'static,
    {
        let handle = self.guard.handle();
        self.runtime.block_on(async move {
            match handle.spawn(fut).await {
                Ok(output) => output,
                Err(err) if err.is_panic() => std::panic::resume_unwind(err.into_panic()),
                Err(_) => unreachable!("task cannot be cancelled inside block_on"),
            }
        })
    }
}

impl TryFrom<crate::Dial9Config> for TracedRuntime {
    type Error = TelemetryRuntimeError;

    fn try_from(config: crate::Dial9Config) -> Result<Self, Self::Error> {
        let (runtime, guard) = try_assemble_dial9_config(config.0)?;
        Ok(Self { runtime, guard })
    }
}

/// Bridge for the deprecated positional config API at
/// [`crate::config::Dial9Config`] so that it remains compatible with
/// [`TracedRuntime::new`] (and therefore the
/// `#[dial9_tokio_telemetry::main]` macro).
impl TryFrom<crate::config::Dial9Config> for TracedRuntime {
    type Error = std::io::Error;

    fn try_from(config: crate::config::Dial9Config) -> Result<Self, Self::Error> {
        let (runtime, guard) = config.build()?;
        Ok(Self {
            runtime,
            guard: guard.unwrap_or_else(TelemetryGuard::disabled),
        })
    }
}

#[cfg(all(test, not(shuttle)))]
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

    /// Nested `InstrumentedSpawnGuard`s must compose: inner drop must not
    /// clear the outer scope. Counter, not flag.
    #[test]
    fn instrumented_spawn_guard_nests() {
        assert_eq!(INSTRUMENTED_SPAWN.with(|c| c.get()), 0);
        let outer = InstrumentedSpawnGuard::enter();
        assert_eq!(INSTRUMENTED_SPAWN.with(|c| c.get()), 1);
        {
            let _inner = InstrumentedSpawnGuard::enter();
            assert_eq!(INSTRUMENTED_SPAWN.with(|c| c.get()), 2);
        }
        assert_eq!(INSTRUMENTED_SPAWN.with(|c| c.get()), 1);
        drop(outer);
        assert_eq!(INSTRUMENTED_SPAWN.with(|c| c.get()), 0);
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
        let _shared = SharedState::new(crate::telemetry::events::clock_monotonic_ns(), None);
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

        // No flush thread or worker to join — the guard is in its
        // disabled state.
        assert!(!guard.is_enabled());
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
            let ts = (i as u64 + 1) * 1000;
            buffer::with_encoder(
                |enc| {
                    let spawn_loc = enc.intern_location(loc);
                    enc.encode(&crate::telemetry::format::TaskSpawnEvent {
                        timestamp_ns: ts,
                        task_id,
                        spawn_loc,
                        instrumented: true,
                    });
                },
                &collector,
                &drain_epoch,
            );
            buffer::with_encoder(
                |enc| {
                    let spawn_loc = enc.intern_location(loc);
                    enc.encode(&crate::telemetry::format::PollStartEvent {
                        timestamp_ns: ts,
                        worker_id: WorkerId::from(0usize),
                        local_queue: 0,
                        task_id,
                        spawn_loc,
                    });
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
                | crate::telemetry::events::TelemetryEvent::WorkerUnpark { worker_id, .. }
                    if *worker_id != WorkerId::UNKNOWN =>
                {
                    worker_ids.insert(worker_id.as_u64());
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

        // At least one segment's metadata should contain both runtime mappings
        // with the exact worker IDs (eagerly populated at attach time).
        let has_both = all_metadata.iter().any(|entries| {
            let has_main = entries
                .iter()
                .any(|(k, v)| k == "runtime.main" && v == "0,1");
            let has_io = entries.iter().any(|(k, v)| k == "runtime.io" && v == "2,3");
            has_main && has_io
        });
        assert!(
            has_both,
            "expected segment metadata to contain runtime.main=0,1 and runtime.io=2,3, \
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

        // Use handle.spawn on runtime B to get wake-tracked wrapping → wake events.
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
                        cpu: None,
                    };
                    *timestamp += 1;
                    ew.write_raw_event(&data).unwrap();
                }
            }

            for op in &round.raw_ops {
                if let FlushOp::PollStart { location_idx } = op {
                    let loc = locations[*location_idx];
                    let task_id = TaskId::from_u32(*timestamp as u32);
                    let ts = *timestamp;
                    *timestamp += 1;

                    ew.write_raw_event(&runtime_context::PollStart {
                        timestamp_ns: ts,
                        worker_id: WorkerId::from(0usize),
                        local_queue: 0,
                        task_id,
                        location: loc,
                    })
                    .unwrap();
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
        assert!(guard.is_enabled());
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
            &guard
                .handle()
                .traced_handle()
                .expect("enabled handle must yield a TracedHandle")
                .shared
                .collector,
        );

        drop(runtime);
        drop(guard);

        // Verify wake events were recorded (handle.spawn wraps with wake tracking)
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

    // ---------------------------------------------------------------
    // High-level construction tests (TracedRuntime::new / try_new)
    // ---------------------------------------------------------------

    fn dial9_config_tmp_base_path() -> std::path::PathBuf {
        let dir = tempfile::tempdir().expect("tempdir");
        // Leak the TempDir so it isn't deleted while the test runs.
        let path = dir.path().join("trace.bin");
        std::mem::forget(dir);
        path
    }

    #[test]
    fn try_new_enabled_path_returns_value_and_exposes_guard() {
        let cfg = crate::Dial9Config::builder()
            .base_path(dial9_config_tmp_base_path())
            .max_file_size(1024 * 1024)
            .max_total_size(4 * 1024 * 1024)
            .build()
            .expect("strict build should succeed");
        let rt = TracedRuntime::try_new(cfg).expect("runtime should build");
        assert!(
            rt.guard().is_enabled(),
            "enabled config must install a live guard"
        );
        // Smoke-test the runtime accessor — exists and is usable.
        let _ = rt.runtime().handle();
        let value = rt.block_on(async { 5u32 });
        assert_eq!(value, 5);
    }

    #[test]
    fn try_new_disabled_path_returns_value_no_guard() {
        let cfg = crate::Dial9Config::builder()
            .enabled(false)
            .build()
            .expect("disabled build should succeed");
        let rt = TracedRuntime::try_new(cfg).expect("disabled runtime should build");
        assert!(
            !rt.guard().is_enabled(),
            "disabled config must yield an inert guard"
        );
        let value = rt.block_on(async { 11u32 });
        assert_eq!(value, 11);
    }

    #[test]
    fn new_returns_runtime_for_valid_disabled_config() {
        // Happy-path counterpart to the strict-I/O panic story: when the
        // config is valid `TracedRuntime::new` returns a usable runtime
        // without panicking. The matching panic path is covered by hand at
        // the type level — `new` is a thin wrapper around `try_into()` that
        // calls `unwrap_or_else(|e| panic!(...))`, and the surrounding
        // tests assert that the inner `TelemetryRuntimeError` formats
        // through `Display` correctly.
        let cfg = crate::Dial9Config::builder()
            .enabled(false)
            .build()
            .expect("disabled build should succeed");
        let rt = TracedRuntime::new(cfg);
        let value = rt.block_on(async { 13u32 });
        assert_eq!(value, 13);
    }

    #[test]
    fn telemetry_runtime_error_display_and_source_chain() {
        let inner = std::io::Error::other("boom");
        let err = TelemetryRuntimeError::TelemetryCore(inner);
        let display = format!("{err}");
        assert!(
            display.contains("telemetry core:"),
            "Display should label the variant, got: {display}"
        );
        assert!(
            display.contains("boom"),
            "Display should include the inner io::Error message, got: {display}"
        );
        let source = std::error::Error::source(&err);
        assert!(source.is_some(), "source() must return the inner io::Error");
    }

    // ---------------------------------------------------------------
    // Always-present TelemetryGuard / inert TelemetryHandle (Phase 3)
    // ---------------------------------------------------------------

    /// Off-runtime callers should get a usable, inert handle rather
    /// than a panic.
    #[test]
    fn telemetry_handle_current_off_runtime_returns_inert_handle() {
        // We're on the test thread, which is not owned by any dial9
        // runtime. `current()` used to panic here.
        let handle = TelemetryHandle::current();
        assert!(
            !handle.is_enabled(),
            "off-runtime current() must return an inert handle"
        );
        // No-op control methods must not panic.
        handle.enable();
        handle.disable();
    }

    /// `TelemetryHandle::disabled` is the explicit constructor for an
    /// inert handle.
    #[test]
    fn telemetry_handle_disabled_constructor_is_inert() {
        let handle = TelemetryHandle::disabled();
        assert!(!handle.is_enabled());
    }

    /// Spawning through a disabled handle still resolves the future —
    /// it just falls through to plain `tokio::spawn` without wake
    /// tracking.
    #[test]
    fn disabled_handle_spawn_falls_through_to_tokio_spawn() {
        let runtime = tokio::runtime::Builder::new_multi_thread()
            .enable_all()
            .build()
            .unwrap();
        let handle = TelemetryHandle::disabled();
        let result = runtime.block_on(async move {
            handle
                .spawn(async { 17u32 })
                .await
                .expect("disabled spawn must still resolve")
        });
        assert_eq!(result, 17);
    }

    /// A disabled guard's `graceful_shutdown` must be a successful
    /// no-op — there is no flush thread or background worker to drain.
    #[test]
    fn disabled_guard_graceful_shutdown_is_noop_ok() {
        let guard = TelemetryGuard::disabled();
        assert!(!guard.is_enabled());
        guard
            .graceful_shutdown(std::time::Duration::from_secs(1))
            .expect("graceful_shutdown on disabled guard must be Ok(())");
    }

    /// The guard returned from a disabled `Dial9Config` is always
    /// present, exposes an inert handle, and reports `is_enabled() ==
    /// false`.
    #[test]
    fn disabled_dial9_config_yields_inert_guard() {
        let cfg = crate::Dial9Config::builder()
            .enabled(false)
            .build()
            .expect("disabled build should succeed");
        let rt = TracedRuntime::try_new(cfg).expect("disabled runtime should build");

        let guard = rt.guard();
        assert!(!guard.is_enabled());
        let handle = guard.handle();
        assert!(!handle.is_enabled());
        // start_time is None on a disabled guard.
        assert!(guard.start_time().is_none());
        // The runtime still works end-to-end.
        let value = rt.block_on(async { 21u32 });
        assert_eq!(value, 21);
    }

    #[cfg(feature = "worker-s3")]
    #[test]
    fn with_s3_client_then_with_s3_uploader_preserves_client() {
        use crate::background_task::s3::S3Config;

        fn dummy_client() -> aws_sdk_s3::Client {
            let conf = aws_sdk_s3::Config::builder()
                .behavior_version_latest()
                .credentials_provider(aws_sdk_s3::config::Credentials::new(
                    "test", "test", None, None, "test",
                ))
                .region(aws_sdk_s3::config::Region::new("us-east-1"))
                .build();
            aws_sdk_s3::Client::from_conf(conf)
        }

        fn cfg(boot_id: &str) -> S3Config {
            S3Config::builder()
                .bucket("b")
                .service_name("s")
                .boot_id(boot_id)
                .build()
        }

        // Order A: client set after the uploader — already worked.
        let mut builder = TracedRuntime::builder()
            .with_s3_uploader(cfg("a"))
            .with_s3_client(dummy_client());
        match &mut builder.pipeline {
            PipelineConfig::S3(u) => {
                assert!(
                    u.take_client().is_some(),
                    "client must be present in order A"
                );
            }
            _ => panic!("expected S3 pipeline"),
        }

        // Order B: client set first, then a follow-up `with_s3_uploader`. The
        // replacement must carry the previously-bound client across.
        let mut builder = TracedRuntime::builder()
            .with_s3_uploader(cfg("a"))
            .with_s3_client(dummy_client())
            .with_s3_uploader(cfg("b"));
        match &mut builder.pipeline {
            PipelineConfig::S3(u) => {
                assert!(
                    u.take_client().is_some(),
                    "client bound before the second with_s3_uploader must be carried over"
                );
            }
            _ => panic!("expected S3 pipeline"),
        }
    }

    /// Pin which builder paths populate `segment_metadata` (the static
    /// entries the writer embeds as a `SegmentMetadata` event in every
    /// sealed segment file). Today the S3 preset auto-injects;
    /// `with_custom_pipeline` does not, so users on that path opt in via
    /// `with_segment_metadata`.
    mod segment_metadata_routing {
        use super::*;

        fn entries<P, M>(builder: &TracedRuntimeBuilder<P, M>) -> &[(String, String)] {
            &builder.segment_metadata
        }

        #[cfg(feature = "worker-s3")]
        fn s3_cfg() -> crate::background_task::s3::S3Config {
            crate::background_task::s3::S3Config::builder()
                .bucket("test-bucket")
                .service_name("checkout-api")
                .instance_path("us-east-1/i-0abc123")
                .boot_id("test-boot")
                .build()
        }

        #[cfg(feature = "worker-s3")]
        #[test]
        fn s3_preset_populates_from_config() {
            let builder = TracedRuntime::builder().with_s3_uploader(s3_cfg());
            let m: std::collections::HashMap<&str, &str> = entries(&builder)
                .iter()
                .map(|(k, v)| (k.as_str(), v.as_str()))
                .collect();
            assert_eq!(m.get("bucket"), Some(&"test-bucket"));
            assert_eq!(m.get("service_name"), Some(&"checkout-api"));
            assert_eq!(m.get("instance_path"), Some(&"us-east-1/i-0abc123"));
            assert_eq!(m.get("boot_id"), Some(&"test-boot"));
        }

        #[cfg(feature = "worker-s3")]
        #[test]
        fn s3_preset_replace_overwrites_metadata() {
            let cfg2 = crate::background_task::s3::S3Config::builder()
                .bucket("other-bucket")
                .service_name("other-svc")
                .boot_id("other-boot")
                .build();
            let builder = TracedRuntime::builder()
                .with_s3_uploader(s3_cfg())
                .with_s3_uploader(cfg2);
            let m: std::collections::HashMap<&str, &str> = entries(&builder)
                .iter()
                .map(|(k, v)| (k.as_str(), v.as_str()))
                .collect();
            // cfg2 wins; nothing leaks from the first call.
            assert_eq!(m.get("bucket"), Some(&"other-bucket"));
            assert_eq!(m.get("service_name"), Some(&"other-svc"));
            assert_eq!(m.get("boot_id"), Some(&"other-boot"));
        }

        /// Custom pipeline does NOT auto-populate, even when `b.s3(cfg)` is
        /// composed inside it. Documented behavior — pinned here so a future
        /// change is intentional.
        #[cfg(feature = "worker-s3")]
        #[test]
        fn custom_pipeline_with_s3_does_not_auto_populate() {
            let builder = TracedRuntime::builder().with_custom_pipeline(|b| b.gzip().s3(s3_cfg()));
            assert!(
                entries(&builder).is_empty(),
                "with_custom_pipeline must not auto-inject segment metadata; got {:?}",
                entries(&builder)
            );
        }

        #[test]
        fn custom_pipeline_without_s3_is_empty() {
            let builder = TracedRuntime::builder().with_custom_pipeline(|b| b.gzip().write_back());
            assert!(entries(&builder).is_empty());
        }

        #[test]
        fn unset_pipeline_is_empty() {
            let builder = TracedRuntime::builder();
            assert!(entries(&builder).is_empty());
        }

        /// Custom-pipeline users can recover S3-preset parity by calling
        /// `with_segment_metadata` explicitly.
        #[cfg(feature = "worker-s3")]
        #[test]
        fn with_segment_metadata_recovers_parity_in_custom_pipeline() {
            let cfg = s3_cfg();
            let preset_entries: Vec<(String, String)> = cfg
                .as_metadata()
                .map(|(k, v)| (k.to_string(), v.to_string()))
                .collect();
            let builder = TracedRuntime::builder()
                .with_custom_pipeline(|b| b.gzip().s3(s3_cfg()))
                .with_segment_metadata(preset_entries.clone());
            assert_eq!(entries(&builder), preset_entries.as_slice());
        }

        /// `with_segment_metadata` after `with_s3_uploader` overrides the
        /// preset's injection — last call wins.
        #[cfg(feature = "worker-s3")]
        #[test]
        fn with_segment_metadata_after_s3_overrides_preset() {
            let custom = vec![("env".to_string(), "prod".to_string())];
            let builder = TracedRuntime::builder()
                .with_s3_uploader(s3_cfg())
                .with_segment_metadata(custom.clone());
            assert_eq!(entries(&builder), custom.as_slice());
        }

        /// `with_s3_uploader` after `with_segment_metadata` overwrites the
        /// custom entries — same "last call wins" rule.
        #[cfg(feature = "worker-s3")]
        #[test]
        fn s3_after_with_segment_metadata_overwrites() {
            let builder = TracedRuntime::builder()
                .with_segment_metadata(vec![("env".into(), "prod".into())])
                .with_s3_uploader(s3_cfg());
            let m: std::collections::HashMap<&str, &str> = entries(&builder)
                .iter()
                .map(|(k, v)| (k.as_str(), v.as_str()))
                .collect();
            assert_eq!(m.get("bucket"), Some(&"test-bucket"));
            assert!(
                !m.contains_key("env"),
                "with_s3_uploader should overwrite, not merge"
            );
        }
    }

    /// Regression test for issue #400: `TelemetryCoreBuilder` must expose
    /// `.s3_config()` so callers can configure S3 without going through
    /// `TracedRuntimeBuilder`.
    #[cfg(feature = "worker-s3")]
    #[test]
    fn telemetry_core_builder_s3_config_builds_successfully() {
        use crate::background_task::s3::S3Config;

        let dir = tempfile::tempdir().unwrap();
        let trace_path = dir.path().join("trace.bin");
        let s3 = S3Config::builder().bucket("b").service_name("s").build();

        let guard = TelemetryCore::builder()
            .writer(NullWriter)
            .trace_path(&trace_path)
            .s3_config(s3)
            .build()
            .expect("TelemetryCoreBuilder with s3_config must build");

        assert!(guard.is_enabled());
        let _ = guard.graceful_shutdown(std::time::Duration::from_secs(1));
    }

    /// Regression test: `TelemetryCore::builder()` with `cpu_profiling` but
    /// without `s3_config` must auto-wire the processor pipeline (symbolize +
    /// gzip + write-back) so the background worker is spawned.
    #[cfg(feature = "cpu-profiling")]
    #[test]
    fn telemetry_core_builder_cpu_profiling_auto_wires_processors() {
        use crate::telemetry::cpu_profile::CpuProfilingConfig;

        let dir = tempfile::tempdir().unwrap();
        let trace_path = dir.path().join("trace.bin");

        // Small max_file_size to force rotation quickly.
        let writer = RotatingWriter::new(&trace_path, 4 * 1024, 10 * 1024 * 1024).unwrap();

        let guard = TelemetryCore::builder()
            .writer(writer)
            .trace_path(&trace_path)
            .cpu_profiling(CpuProfilingConfig::default())
            .worker_poll_interval(std::time::Duration::from_millis(50))
            .build()
            .expect("TelemetryCoreBuilder with cpu_profiling must build");

        guard.enable();

        // Attach a runtime and generate enough events to force segment rotation.
        let mut builder = tokio::runtime::Builder::new_multi_thread();
        builder.worker_threads(2).enable_all();
        let (runtime, _handle) = guard.trace_runtime("test").build(builder).unwrap();

        runtime.block_on(async {
            // Generate events to fill the small 4KB segment.
            for _ in 0..1000 {
                tokio::task::yield_now().await;
            }
            // Give the worker time to process the sealed segment.
            tokio::time::sleep(std::time::Duration::from_millis(200)).await;
        });

        runtime.shutdown_timeout(std::time::Duration::from_secs(1));
        let _ = guard.graceful_shutdown(std::time::Duration::from_secs(2));

        // After shutdown, the worker should have processed at least one
        // segment. WriteBackProcessor writes .bin.gz files.
        let gz_files: Vec<_> = std::fs::read_dir(dir.path())
            .unwrap()
            .filter_map(|e| e.ok())
            .filter(|e| e.path().extension().is_some_and(|ext| ext == "gz"))
            .collect();
        assert!(
            !gz_files.is_empty(),
            "cpu_profiling should auto-wire processors that produce .gz files, \
             but no .gz files found in {:?}. Files present: {:?}",
            dir.path(),
            std::fs::read_dir(dir.path())
                .unwrap()
                .filter_map(|e| e.ok())
                .map(|e| e.path())
                .collect::<Vec<_>>()
        );
    }
}
