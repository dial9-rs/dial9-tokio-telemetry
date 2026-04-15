//! `Traced<F>` future wrapper for wake event capture and task dump collection.

use crate::telemetry::recorder::SharedState;
use crate::telemetry::task_metadata::TaskId;
use futures_util::task::{ArcWake, AtomicWaker, waker as arc_waker};
use pin_project_lite::pin_project;
use std::cell::Cell;
use std::future::Future;
use std::pin::Pin;
use std::sync::Arc;
use std::sync::atomic::Ordering;
use std::task::{Context, Poll, Waker};

thread_local! {
    /// Per-task opt-in override for task dump capture.
    /// Set by `enable_task_dumps()`, read and latched by `Traced<F>` on each poll.
    /// Once a `Traced` instance reads `true`, it stores the flag on itself and
    /// the thread-local is cleared so it doesn't leak to other tasks on the same thread.
    static TASK_DUMP_OVERRIDE: Cell<bool> = const { Cell::new(false) };
}

/// Enable task dump capture for the current task.
///
/// Call this inside a `Traced` future to opt in to task dump capture even when
/// global task dumps are disabled. The next poll of the enclosing `Traced`
/// wrapper will latch this flag, enabling dumps for that task only.
///
/// ```rust,no_run
/// dial9_tokio_telemetry::enable_task_dumps();
/// ```
pub fn enable_task_dumps() {
    TASK_DUMP_OVERRIDE.with(|c| c.set(true));
}

/// Read and clear the per-task override. Called by `Traced::poll` to latch
/// the flag onto the per-instance `task_dump_override` field.
fn take_task_dump_override() -> bool {
    TASK_DUMP_OVERRIDE.with(|c| {
        if c.get() {
            c.set(false);
            true
        } else {
            false
        }
    })
}

/// Handle used by `Traced<F>` to emit events into the telemetry system.
/// Obtained via `TelemetryHandle::traced_handle()`.
#[derive(Clone)]
pub struct TracedHandle {
    pub(crate) shared: Arc<SharedState>,
}

impl std::fmt::Debug for TracedHandle {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("TracedHandle").finish_non_exhaustive()
    }
}

pin_project! {
    /// Future wrapper that captures wake events (and later, task dumps).
    pub struct Traced<F> {
        #[pin]
        inner: F,
        handle: TracedHandle,
        task_id: TaskId,
        waker_data: Arc<TracedWakerData>, // reused across polls to avoid a per-poll Arc allocation
        // Task dump state: captured backtraces from yield points
        pending_frames: Vec<Vec<u64>>,
        pending_capture_ts: u64,
        // Per-task opt-in: latched from thread-local on first poll that sees it
        task_dump_override: bool,
    }
}

impl<F> Traced<F> {
    pub(crate) fn new(inner: F, handle: TracedHandle, task_id: TaskId) -> Self {
        let waker_data = Arc::new(TracedWakerData {
            inner: AtomicWaker::new(),
            woken_task_id: task_id,
            shared: handle.shared.clone(),
        });
        Self {
            inner,
            handle,
            task_id,
            waker_data,
            pending_frames: Vec::new(),
            pending_capture_ts: 0,
            task_dump_override: false,
        }
    }
}

// --- Waker wrapping ---

/// Shared state threaded through our custom `Waker`.
///
/// `inner` is an `AtomicWaker` so that the waker registered by the executor
/// can be stored and replaced in a thread-safe way without allocating a new
/// `Arc` on every `poll`.
struct TracedWakerData {
    inner: AtomicWaker,
    woken_task_id: TaskId,
    shared: Arc<SharedState>,
}

impl ArcWake for TracedWakerData {
    fn wake_by_ref(arc_self: &Arc<Self>) {
        record_wake_event(arc_self);
        arc_self.inner.wake();
    }
}

fn record_wake_event(data: &TracedWakerData) {
    // The worker issuing the wake — not the worker that will execute the woken task
    // (which is unknowable at wake time). Stored in the event as `target_worker`.
    let waking_worker = crate::telemetry::recorder::current_worker_id();
    let event = data
        .shared
        .create_wake_event(data.woken_task_id, waking_worker);
    data.shared.record_event(event);
}

fn make_traced_waker(data: Arc<TracedWakerData>) -> Waker {
    arc_waker(data)
}

/// Capture backtraces at yield points using `trace_with`.
///
/// Re-polls the inner future with a no-op waker inside `trace_with`.
/// The callback fires at each yield point, where we use `backtrace::trace`
/// to collect frame instruction pointers.
fn capture_task_dump(inner: Pin<&mut impl Future>, frames: &mut Vec<Vec<u64>>) {
    let noop_waker = Waker::noop();
    let mut noop_cx = Context::from_waker(noop_waker);
    frames.clear();
    let frames_ref = &mut *frames;
    tokio::runtime::dump::trace_with(
        || {
            let _ = inner.poll(&mut noop_cx);
        },
        |_meta| {
            let mut frame_ips = Vec::new();
            backtrace::trace(|frame| {
                frame_ips.push(frame.ip() as u64);
                true
            });
            frames_ref.push(frame_ips);
        },
    );
}

impl<F: Future> Future for Traced<F> {
    type Output = F::Output;

    fn poll(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Self::Output> {
        let mut this = self.project();
        if !this.handle.shared.enabled.load(Ordering::Relaxed) {
            return this.inner.poll(cx);
        }

        // Store (or replace) the executor's waker so that when our custom
        // waker fires it can forward the notification to the correct waker,
        // even if the task has been moved to a different executor thread
        // between polls.
        this.waker_data.inner.register(cx.waker());

        let traced_waker = make_traced_waker(this.waker_data.clone());
        let mut traced_cx = Context::from_waker(&traced_waker);
        let result = this.inner.as_mut().poll(&mut traced_cx);

        match &result {
            Poll::Ready(_) => {
                // TODO(task-dump): If pending_frames is non-empty and elapsed > threshold,
                // emit TaskDumpEvents before dropping. The task completed after a long idle,
                // and we want to capture what it was waiting on.
                this.pending_frames.clear();
            }
            Poll::Pending => {
                // Latch the thread-local override onto this instance so it
                // persists across polls and doesn't leak to other tasks.
                if take_task_dump_override() {
                    *this.task_dump_override = true;
                }
                let dumps_enabled = this
                    .handle
                    .shared
                    .task_dumps_enabled
                    .load(Ordering::Relaxed)
                    || *this.task_dump_override;
                if dumps_enabled {
                    capture_task_dump(this.inner, this.pending_frames);
                    *this.pending_capture_ts = crate::telemetry::events::clock_monotonic_ns();
                }
            }
        }

        result
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::telemetry::buffer;
    use crate::telemetry::events::TelemetryEvent;
    use crate::telemetry::recorder::TracedRuntime;
    use crate::telemetry::task_metadata::UNKNOWN_TASK_ID;
    use crate::telemetry::writer::RotatingWriter;
    use std::sync::{Arc, Mutex};
    use tempfile::TempDir;

    /// Verify that `Traced<F>` records a `WakeEvent` whose `woken_task_id`
    /// matches the spawned task when a `Notify` wakes it.
    ///
    /// This is an integration test: events are written to a real file via
    /// `RotatingWriter` and then read back with `TraceReader`.
    #[test]
    #[cfg(feature = "analysis")]
    fn traced_emits_wake_events() {
        use crate::telemetry::analysis::TraceReader;
        let dir = TempDir::new().unwrap();
        let trace_path = dir.path().join("trace.bin");

        // Build a current-thread runtime so that all tasks — and all thread-local
        // BUFFER accesses — share a single thread with the test itself.
        let (runtime, guard) = TracedRuntime::build_and_start(
            tokio::runtime::Builder::new_current_thread(),
            RotatingWriter::single_file(&trace_path).unwrap(),
        )
        .unwrap();

        let handle = guard.handle();
        let notify = Arc::new(tokio::sync::Notify::new());
        let notify_clone = notify.clone();

        // We'll capture the spawned task's ID from inside the task so we can
        // assert the correct `woken_task_id` appears in the recorded events.
        let spawned_id: Arc<Mutex<TaskId>> = Arc::new(Mutex::new(UNKNOWN_TASK_ID));
        let spawned_id_write = spawned_id.clone();

        runtime.block_on(async {
            // Spawn a task wrapped in Traced that blocks on a Notify.
            let join = handle.spawn(async move {
                *spawned_id_write.lock().unwrap() = tokio::task::try_id()
                    .map(TaskId::from)
                    .unwrap_or(UNKNOWN_TASK_ID);
                notify_clone.notified().await;
            });

            // Yield so the spawned task runs its first poll and registers its
            // waker with the Notify before we send the notification.
            tokio::task::yield_now().await;

            // This calls wake_by_ref on our TracedWakerData, recording the WakeEvent.
            notify.notify_one();

            join.await.unwrap();
        });

        // Wake events land in the thread-local buffer (capacity 1_024), so a
        // single event will not auto-flush.  Manually drain the buffer into the
        // collector so that the guard flush below picks it up.
        let th = handle.traced_handle();
        buffer::drain_to_collector(&th.shared.collector);

        // Dropping the guard stops the background flush thread, joins it, then
        // performs a final flush: collector → RotatingWriter → trace file.
        drop(guard);

        // Parse the trace file and collect all WakeEvents.
        let sealed = dir.path().join("trace.0.bin");
        let trace_path_str = sealed.to_str().unwrap();
        let reader = TraceReader::new(trace_path_str).unwrap();
        let events = &reader.runtime_events;

        let wake_task_ids: Vec<TaskId> = events
            .iter()
            .filter_map(|e| {
                if let TelemetryEvent::WakeEvent { woken_task_id, .. } = e {
                    Some(*woken_task_id)
                } else {
                    None
                }
            })
            .collect();

        assert!(
            !wake_task_ids.is_empty(),
            "expected at least one WakeEvent but got none; all events: {events:#?}"
        );

        let expected = *spawned_id.lock().unwrap();
        assert_ne!(
            expected, UNKNOWN_TASK_ID,
            "spawned task should have a real tokio task ID"
        );
        assert!(
            wake_task_ids.contains(&expected),
            "no WakeEvent with woken_task_id={expected:?}; recorded wake_task_ids={wake_task_ids:?}"
        );
    }

    /// Verify that `Traced<F>` captures backtrace frames at yield points
    /// when task dumps are enabled.
    #[test]
    fn traced_captures_backtrace_on_pending() {
        use std::sync::atomic::Ordering;

        let mut builder = tokio::runtime::Builder::new_current_thread();
        builder.enable_all();
        let (runtime, guard) =
            TracedRuntime::build_and_start(builder, crate::telemetry::writer::NullWriter).unwrap();

        // Enable task dumps
        guard
            .handle()
            .traced_handle()
            .shared
            .task_dumps_enabled
            .store(true, Ordering::Relaxed);

        let handle = guard.handle();

        runtime.block_on(async {
            let join = handle.spawn(async move {
                // Sleep will return Pending on first poll, triggering backtrace capture
                tokio::time::sleep(std::time::Duration::from_millis(50)).await;
            });
            join.await.unwrap();
        });

        // The test verifies the mechanism works by checking the task completes
        // successfully (no panics from trace_with + backtrace). The actual
        // frame content is verified in integration tests that read the trace.
        drop(runtime);
        drop(guard);
    }

    /// Verify that the no-op waker used in trace_with does NOT cause a
    /// duplicate wake. The task should complete normally with exactly the
    /// expected number of wake events.
    #[test]
    #[cfg(feature = "analysis")]
    fn trace_with_noop_waker_no_duplicate_wake() {
        use crate::telemetry::analysis::TraceReader;
        use std::sync::atomic::Ordering;

        let dir = TempDir::new().unwrap();
        let trace_path = dir.path().join("trace.bin");

        let (runtime, guard) = TracedRuntime::build_and_start(
            tokio::runtime::Builder::new_current_thread(),
            RotatingWriter::single_file(&trace_path).unwrap(),
        )
        .unwrap();

        guard
            .handle()
            .traced_handle()
            .shared
            .task_dumps_enabled
            .store(true, Ordering::Relaxed);

        let handle = guard.handle();
        let spawned_id: Arc<Mutex<TaskId>> = Arc::new(Mutex::new(UNKNOWN_TASK_ID));
        let spawned_id_write = spawned_id.clone();

        runtime.block_on(async {
            let join = handle.spawn(async move {
                *spawned_id_write.lock().unwrap() = tokio::task::try_id()
                    .map(TaskId::from)
                    .unwrap_or(UNKNOWN_TASK_ID);
                // Single yield point — should produce exactly one wake
                tokio::task::yield_now().await;
            });
            join.await.unwrap();
        });

        drop(guard);

        let sealed = dir.path().join("trace.0.bin");
        let reader = TraceReader::new(sealed.to_str().unwrap()).unwrap();
        let events = &reader.runtime_events;

        let expected_id = *spawned_id.lock().unwrap();
        let wake_count = events
            .iter()
            .filter(|e| {
                matches!(e, TelemetryEvent::WakeEvent { woken_task_id, .. } if *woken_task_id == expected_id)
            })
            .count();

        // yield_now produces exactly one wake. If trace_with caused a
        // duplicate wake, we'd see 2.
        assert_eq!(
            wake_count, 1,
            "expected exactly 1 wake for yield_now task, got {wake_count}; \
             trace_with with noop waker should not trigger extra wakes"
        );
    }

    /// Verify that trace_with re-poll does NOT produce extra PollStart/PollEnd
    /// events. The same workload with and without task dumps must produce
    /// identical poll event counts.
    #[test]
    #[cfg(feature = "analysis")]
    fn task_dump_does_not_produce_extra_poll_events() {
        use crate::telemetry::analysis::TraceReader;
        use std::sync::atomic::Ordering;

        fn run_workload(enable_dumps: bool) -> (usize, usize) {
            let dir = TempDir::new().unwrap();
            let trace_path = dir.path().join("trace.bin");

            let mut builder = tokio::runtime::Builder::new_current_thread();
            builder.enable_all();
            let (runtime, guard) = TracedRuntime::build_and_start(
                builder,
                RotatingWriter::single_file(&trace_path).unwrap(),
            )
            .unwrap();

            if enable_dumps {
                guard
                    .handle()
                    .traced_handle()
                    .shared
                    .task_dumps_enabled
                    .store(true, Ordering::Relaxed);
            }

            let handle = guard.handle();

            runtime.block_on(async {
                let join = handle.spawn(async {
                    tokio::task::yield_now().await;
                    tokio::task::yield_now().await;
                    tokio::task::yield_now().await;
                });
                join.await.unwrap();
            });

            drop(runtime);
            drop(guard);

            let sealed = dir.path().join("trace.0.bin");
            let reader = TraceReader::new(sealed.to_str().unwrap()).unwrap();
            let events = &reader.runtime_events;

            let starts = events
                .iter()
                .filter(|e| matches!(e, TelemetryEvent::PollStart { .. }))
                .count();
            let ends = events
                .iter()
                .filter(|e| matches!(e, TelemetryEvent::PollEnd { .. }))
                .count();
            (starts, ends)
        }

        let (starts_off, ends_off) = run_workload(false);
        let (starts_on, ends_on) = run_workload(true);

        assert_eq!(
            starts_off, starts_on,
            "PollStart count differs: without dumps={starts_off}, with dumps={starts_on}"
        );
        assert_eq!(
            ends_off, ends_on,
            "PollEnd count differs: without dumps={ends_off}, with dumps={ends_on}"
        );
    }
}
