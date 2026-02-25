//! `Traced<F>` future wrapper for wake event capture and task dump collection.

use crate::telemetry::recorder::SharedState;
use crate::telemetry::task_metadata::TaskId;
use futures_util::task::{ArcWake, AtomicWaker, waker as arc_waker};
use pin_project_lite::pin_project;
use std::future::Future;
use std::pin::Pin;
use std::sync::Arc;
use std::sync::atomic::Ordering;
use std::task::{Context, Poll, Waker};

/// Handle used by `Traced<F>` to emit events into the telemetry system.
/// Obtained via `TelemetryHandle::traced_handle()`.
#[derive(Clone)]
pub struct TracedHandle {
    pub(crate) shared: Arc<SharedState>,
}

pin_project! {
    /// Future wrapper that captures wake events (and later, task dumps).
    pub struct Traced<F> {
        #[pin]
        inner: F,
        handle: TracedHandle,
        task_id: TaskId,
        waker_data: Arc<TracedWakerData>, // reused across polls to avoid a per-poll Arc allocation
    }
}

impl<F> Traced<F> {
    pub fn new(inner: F, handle: TracedHandle, task_id: TaskId) -> Self {
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
    let event = data.shared.create_wake_event(data.woken_task_id);
    data.shared.record_event(event);
}

fn make_traced_waker(data: Arc<TracedWakerData>) -> Waker {
    arc_waker(data)
}

impl<F: Future> Future for Traced<F> {
    type Output = F::Output;

    fn poll(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Self::Output> {
        let this = self.project();
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
        this.inner.poll(&mut traced_cx)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::telemetry::analysis::TraceReader;
    use crate::telemetry::buffer::BUFFER;
    use crate::telemetry::events::TelemetryEvent;
    use crate::telemetry::recorder::TracedRuntime;
    use crate::telemetry::task_metadata::UNKNOWN_TASK_ID;
    use crate::telemetry::writer::SimpleBinaryWriter;
    use std::sync::{Arc, Mutex};
    use tempfile::TempDir;

    /// Verify that `Traced<F>` records a `WakeEvent` whose `woken_task_id`
    /// matches the spawned task when a `Notify` wakes it.
    ///
    /// This is an integration test: events are written to a real file via
    /// `SimpleBinaryWriter` and then read back with `TraceReader`.
    #[test]
    fn traced_emits_wake_events() {
        let dir = TempDir::new().unwrap();
        let trace_path = dir.path().join("trace.bin");

        // Build a current-thread runtime so that all tasks — and all thread-local
        // BUFFER accesses — share a single thread with the test itself.
        let (runtime, guard) = TracedRuntime::build_and_start(
            tokio::runtime::Builder::new_current_thread(),
            Box::new(SimpleBinaryWriter::new(&trace_path).unwrap()),
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

        // Wake events land in the thread-local BUFFER (capacity 1_024), so a
        // single event will not auto-flush.  Manually drain the buffer into the
        // collector so that the guard flush below picks it up.
        let th = handle.traced_handle();
        BUFFER.with(|buf| {
            let batch = buf.borrow_mut().flush();
            if !batch.is_empty() {
                th.shared.collector.accept_flush(batch);
            }
        });

        // Dropping the guard stops the background flush thread, joins it, then
        // performs a final flush: collector → SimpleBinaryWriter → trace file.
        drop(guard);

        // Parse the trace file and collect all WakeEvents.
        let trace_path_str = trace_path.to_str().unwrap();
        let mut reader = TraceReader::new(trace_path_str).unwrap();
        reader.read_header().unwrap();
        let events = reader.read_all().unwrap();

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
}
