//! `Traced<F>` future wrapper for wake event capture and task dump collection.

use crate::telemetry::buffer::BUFFER;
use crate::telemetry::events::RawEvent;
use crate::telemetry::recorder::{SharedState, current_worker_id};
use crate::telemetry::task_metadata::TaskId;
use pin_project_lite::pin_project;
use std::future::Future;
use std::pin::Pin;
use std::sync::atomic::Ordering;
use std::sync::Arc;
use std::task::{Context, Poll, RawWaker, RawWakerVTable, Waker};

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
    }
}

impl<F> Traced<F> {
    pub fn new(inner: F, handle: TracedHandle, task_id: TaskId) -> Self {
        Self { inner, handle, task_id }
    }
}

// --- Waker wrapping ---

struct TracedWakerData {
    inner: Waker,
    woken_task_id: TaskId,
    shared: Arc<SharedState>,
}

const VTABLE: RawWakerVTable = RawWakerVTable::new(
    traced_waker_clone,
    traced_waker_wake,
    traced_waker_wake_by_ref,
    traced_waker_drop,
);

unsafe fn traced_waker_clone(data: *const ()) -> RawWaker {
    let arc = unsafe { Arc::from_raw(data as *const TracedWakerData) };
    let cloned = arc.clone();
    std::mem::forget(arc);
    RawWaker::new(Arc::into_raw(cloned) as *const (), &VTABLE)
}

unsafe fn traced_waker_wake(data: *const ()) {
    let arc = unsafe { Arc::from_raw(data as *const TracedWakerData) };
    record_wake_event(&arc);
    arc.inner.wake_by_ref();
}

unsafe fn traced_waker_wake_by_ref(data: *const ()) {
    let arc = unsafe { Arc::from_raw(data as *const TracedWakerData) };
    record_wake_event(&arc);
    arc.inner.wake_by_ref();
    std::mem::forget(arc);
}

unsafe fn traced_waker_drop(data: *const ()) {
    let _arc = unsafe { Arc::from_raw(data as *const TracedWakerData) };
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

fn make_traced_waker(
    inner: &Waker,
    woken_task_id: TaskId,
    shared: &Arc<SharedState>,
) -> Waker {
    let data = Arc::new(TracedWakerData {
        inner: inner.clone(),
        woken_task_id,
        shared: shared.clone(),
    });
    let raw = RawWaker::new(Arc::into_raw(data) as *const (), &VTABLE);
    // SAFETY: Our vtable correctly implements clone/wake/drop for Arc<TracedWakerData>
    unsafe { Waker::from_raw(raw) }
}

impl<F: Future> Future for Traced<F> {
    type Output = F::Output;

    fn poll(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Self::Output> {
        let this = self.project();
        if !this.handle.shared.enabled.load(Ordering::Relaxed) {
            return this.inner.poll(cx);
        }

        let traced_waker = make_traced_waker(cx.waker(), *this.task_id, &this.handle.shared);
        let mut traced_cx = Context::from_waker(&traced_waker);
        this.inner.poll(&mut traced_cx)
    }
}
