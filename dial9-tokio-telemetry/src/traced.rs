//! `Traced<F>` future wrapper for wake event capture and task dump collection.

use crate::telemetry::buffer::BUFFER;
use crate::telemetry::events::RawEvent;
use crate::telemetry::recorder::{SharedState, current_worker_id};
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
    if !data.shared.enabled.load(Ordering::Relaxed) {
        return;
    }
    let waker_task_id = tokio::task::try_id()
        .map(TaskId::from)
        .unwrap_or(TaskId::from_u32(0));
    let target_worker = current_worker_id(&data.shared.metrics);
    let timestamp_nanos = data.shared.start_time.elapsed().as_nanos() as u64;

    let event = RawEvent::WakeEvent {
        timestamp_nanos,
        waker_task_id,
        woken_task_id: data.woken_task_id,
        target_worker,
    };
    BUFFER.with(|buf| {
        let mut buf = buf.borrow_mut();
        buf.record_event(event);
        if buf.should_flush() {
            data.shared.collector.accept_flush(buf.flush());
        }
    });
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
