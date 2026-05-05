//! `TaskDumped<F>` wraps a future and captures async backtraces at yield
//! points when the task stays idle longer than the configured threshold.
//!
//! This wrapper is intentionally separate from [`crate::traced::Traced`]: the
//! wake-event capture in `Traced` runs on every instrumented spawn regardless
//! of the `taskdump` feature, while task-dump capture is gated behind the
//! `taskdump` feature and its own runtime toggle.  Typical stacking is
//! `Traced<TaskDumped<F>>`.
//!
//! # Capture model
//!
//! On each poll, the wrapper inspects the elapsed time since the last capture
//! point (i.e., the task's idle duration leading up to this poll). If that
//! idle exceeded the configured threshold, frames captured at the *previous*
//! yield point are emitted as `TaskDump` events. If the current poll returns
//! `Pending`, a fresh capture is taken via [`tokio::runtime::dump::trace_with`]
//! so that the next poll's decision has fresh data.
//!
//! The capture runs a second `poll` of the inner future under a no-op waker
//! inside `trace_with`. Tokio yield points use the *inner* context's waker
//! (noop) rather than the real executor waker, so this does not produce a
//! duplicate `WakeEvent`, and the `PollStart`/`PollEnd` hooks run only on the
//! outer scheduler call, not on the trace_with sub-poll.
//!
//! # Allocation
//!
//! Captured instruction pointers are stored flat in [`FrameBuf`] across all
//! yield points hit during a capture, with offsets recording each callchain's
//! start. The buffers are reused across polls.

use crate::telemetry::format::TaskDumpEvent;
use crate::telemetry::recorder::SharedState;
use crate::telemetry::task_metadata::TaskId;
use crate::telemetry::{Encodable, ThreadLocalEncoder};
use pin_project_lite::pin_project;
use smallvec::SmallVec;
use std::future::Future;
use std::pin::Pin;
use std::sync::Arc;
use std::sync::atomic::Ordering;
use std::task::{Context, Poll, Waker};

/// Initial heap reservation for the instruction-pointer buffer on first capture.
const FRAME_BUF_INITIAL_CAPACITY: usize = 256;

pin_project! {
    /// Future wrapper that captures async backtraces at yield points when
    /// the enclosing task has been idle longer than the configured threshold.
    pub(crate) struct TaskDumped<F> {
        #[pin]
        inner: F,
        shared: Arc<SharedState>,
        task_id: TaskId,
        frames: FrameBuf,
        // Monotonic nanoseconds when the frames in `frames` were captured.
        // Only meaningful when `frames.has_data()`.
        pending_capture_ts: u64,
        // Snapshot of SharedState::task_dump_idle_threshold_ns at construction;
        // promotes the atomic load off the poll hot path.
        idle_threshold_ns: u64,
    }
}

impl<F> TaskDumped<F> {
    pub(crate) fn new(inner: F, shared: Arc<SharedState>, task_id: TaskId) -> Self {
        let idle_threshold_ns = shared.task_dump_idle_threshold_ns.load(Ordering::Relaxed);
        Self {
            inner,
            shared,
            task_id,
            frames: FrameBuf::new(),
            pending_capture_ts: 0,
            idle_threshold_ns,
        }
    }
}

impl<F: Future> Future for TaskDumped<F> {
    type Output = F::Output;

    fn poll(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<F::Output> {
        let mut this = self.project();

        // Fast path: forward without any capture work when either task dumps
        // are disabled, or telemetry as a whole is paused. Checking both here
        // skips the re-poll under `trace_with` (not just the emit), so a
        // paused guard imposes no overhead beyond two relaxed atomic loads.
        if !this.shared.task_dumps_enabled.load(Ordering::Relaxed) || !this.shared.is_enabled() {
            // Stale pending frames become meaningless if we stopped capturing —
            // drop them so we don't emit a dump attributed to an old idle gap
            // after capture resumes.
            if this.frames.has_data() {
                this.frames.clear();
                *this.pending_capture_ts = 0;
            }
            return this.inner.poll(cx);
        }

        // If we have captured frames from a previous idle, decide whether
        // that idle was long enough to emit.
        let should_emit = if this.frames.has_data() {
            let now = crate::telemetry::events::clock_monotonic_ns();
            now.saturating_sub(*this.pending_capture_ts) > *this.idle_threshold_ns
        } else {
            false
        };

        let result = this.inner.as_mut().poll(cx);

        if should_emit {
            this.frames
                .emit(this.shared, *this.task_id, *this.pending_capture_ts);
        }

        match &result {
            Poll::Ready(_) => {
                // Terminal. Nothing more to capture; discard stale frames.
                this.frames.clear();
                *this.pending_capture_ts = 0;
            }
            Poll::Pending => {
                // Capture the yield point we just landed on, for the next
                // poll's threshold check.
                this.frames.capture(this.inner.as_mut());
                *this.pending_capture_ts = crate::telemetry::events::clock_monotonic_ns();
            }
        }

        result
    }
}

/// Reusable storage for one or more callchains captured during a single
/// `trace_with` sub-poll. Frames are appended flat to `ips`; each new chain's
/// start index is pushed onto `offsets`.
struct FrameBuf {
    ips: Vec<u64>,
    offsets: SmallVec<[usize; 8]>,
}

impl FrameBuf {
    fn new() -> Self {
        Self {
            ips: Vec::new(),
            offsets: SmallVec::new(),
        }
    }

    fn clear(&mut self) {
        self.ips.clear();
        self.offsets.clear();
    }

    fn has_data(&self) -> bool {
        !self.offsets.is_empty()
    }

    /// Emit one `TaskDumpEvent` per recorded callchain, then clear.
    fn emit(&mut self, shared: &SharedState, task_id: TaskId, capture_ts: u64) {
        shared.if_enabled(|buf| {
            for i in 0..self.offsets.len() {
                let start = self.offsets[i];
                let end = self.offsets.get(i + 1).copied().unwrap_or(self.ips.len());
                buf.record_encodable_event(&TaskDumpData {
                    timestamp_ns: capture_ts,
                    task_id,
                    callchain: &self.ips[start..end],
                });
            }
        });
        self.clear();
    }

    /// Capture backtraces at yield points by re-polling `inner` under a no-op
    /// waker inside `trace_with`.
    fn capture<F: Future>(&mut self, inner: Pin<&mut F>) {
        if self.ips.capacity() == 0 {
            self.ips.reserve(FRAME_BUF_INITIAL_CAPACITY);
        }
        self.clear();

        // Noop waker so any waker registration performed during this
        // diagnostic re-poll is discarded, avoiding duplicate wake events.
        let noop = Waker::noop();
        let mut noop_cx = Context::from_waker(noop);
        let ips = &mut self.ips;
        let offsets = &mut self.offsets;

        // `trace_with`'s outer closure is `FnOnce`; `Option::take` moves the
        // pinned reference in without requiring a `Copy` bound or unsafe.
        tokio::runtime::dump::trace_with(
            || {
                let _ = inner.poll(&mut noop_cx);
            },
            |meta| {
                offsets.push(ips.len());
                capture_frames(ips, meta.root_addr, meta.trace_leaf_addr);
            },
        );
    }
}

/// Walk the stack via [`backtrace::trace`], skipping frames at or below
/// `leaf_addr` (tokio's `trace_leaf` function — any frames at or below it
/// are internal plumbing) and stopping at `root_addr` (tokio's `Root::poll`
/// boundary). `root_addr` is always `None` today because we don't wrap
/// spawned tasks in `Root`, but we keep the check defensively in case that
/// changes — unwrapping the whole stack otherwise walks through scheduler
/// internals that the caller has no use for.
///
/// Held behind the `backtrace` crate's process-wide mutex. This will be optimized in
/// https://github.com/dial9-rs/dial9-tokio-telemetry/issues/357. This is not a
/// complete blocker for the MvP because no code is utilizing the symbolizing from
/// `backtrace` so the mutex is not actually held for long periods of time. Nevertheless,
/// this will be a fast follow.
fn capture_frames(
    ips: &mut Vec<u64>,
    root_addr: Option<*const core::ffi::c_void>,
    leaf_addr: *const core::ffi::c_void,
) {
    let mut above_leaf = false;
    backtrace::trace(|frame| {
        let sym = frame.symbol_address();
        let below_root = root_addr.is_none_or(|root| !std::ptr::eq(sym, root));

        if above_leaf && below_root {
            ips.push(frame.ip() as u64);
        }

        if std::ptr::eq(sym, leaf_addr) {
            above_leaf = true;
        }

        below_root
    });
}

/// Borrowed-callchain view of a task-dump event that implements [`Encodable`]
/// by interning its ips into the batch's stack pool.
pub(crate) struct TaskDumpData<'a> {
    pub(crate) timestamp_ns: u64,
    pub(crate) task_id: TaskId,
    pub(crate) callchain: &'a [u64],
}

impl Encodable for TaskDumpData<'_> {
    fn encode(&self, enc: &mut ThreadLocalEncoder<'_>) {
        let interned_callchain = enc.intern_stack_frames(self.callchain);
        enc.encode(&TaskDumpEvent {
            timestamp_ns: self.timestamp_ns,
            task_id: self.task_id,
            callchain: interned_callchain,
        });
    }
}
