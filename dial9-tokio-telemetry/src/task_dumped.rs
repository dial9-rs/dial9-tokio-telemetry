//! `TaskDumped<F>` wraps a future and captures async backtraces at yield
//! points using geometric (Poisson) sampling keyed on idle duration.
//!
//! This wrapper is intentionally separate from [`crate::traced::Traced`]: the
//! wake-event capture in `Traced` runs on every instrumented spawn regardless
//! of the `taskdump` feature, while task-dump capture is gated behind the
//! `taskdump` feature and its own runtime toggle.  Typical stacking is
//! `Traced<TaskDumped<F>>`.
//!
//! # Sampling model
//!
//! Instead of a hard time cutoff, each task maintains a byte-counter–style
//! `next_sample_ns` drawn from an exponential distribution with mean equal to
//! the configured `idle_threshold`. On each poll, the preceding idle duration
//! is subtracted from the counter. When the counter reaches zero or below, the
//! captured frames are emitted and a new gap is drawn. This gives unbiased
//! Poisson sampling: longer idles are more likely to trigger a dump, but even
//! short idles have a non-zero (if small) probability.
//!
//! # Capture mechanics
//!
//! If the current poll returns `Pending`, a fresh capture is taken via
//! [`tokio::runtime::dump::trace_with`] so that the next poll's sampling
//! decision has fresh data. The capture runs a second `poll` of the inner
//! future under a no-op waker inside `trace_with`. Tokio yield points use the
//! *inner* context's waker (noop) rather than the real executor waker, so this
//! does not produce a duplicate `WakeEvent`, and the `PollStart`/`PollEnd`
//! hooks run only on the outer scheduler call, not on the trace_with sub-poll.
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
use std::num::NonZeroU64;
use std::pin::Pin;
use std::sync::Arc;
use std::sync::atomic::Ordering;
use std::task::{Context, Poll, Waker};

/// Initial heap reservation for the instruction-pointer buffer on first capture.
const FRAME_BUF_INITIAL_CAPACITY: usize = 256;

// ─── Minimal PRNG (splitmix64) ──────────────────────────────────────────────

/// Minimal splitmix64 PRNG. Fast, no dependencies, good enough for sampling.
struct SplitMix64(u64);

impl SplitMix64 {
    fn new(seed: u64) -> Self {
        Self(seed)
    }

    fn next_u64(&mut self) -> u64 {
        self.0 = self.0.wrapping_add(0x9e3779b97f4a7c15);
        let mut z = self.0;
        z = (z ^ (z >> 30)).wrapping_mul(0xbf58476d1ce4e5b9);
        z = (z ^ (z >> 27)).wrapping_mul(0x94d049bb133111eb);
        z ^ (z >> 31)
    }

    /// Draw from exponential distribution with given mean (in nanoseconds).
    /// Returns at least 1 to avoid immediate re-trigger.
    fn draw_exponential_ns(&mut self, mean_ns: u64) -> i64 {
        // Generate a uniform float in (0, 1] — avoid exact 0 to prevent ln(0).
        let u = (self.next_u64() >> 11) as f64 / ((1u64 << 53) as f64);
        let u = if u == 0.0 { f64::MIN_POSITIVE } else { u };
        let sample = -u.ln() * (mean_ns as f64);
        // Clamp to at least 1ns so we never immediately re-trigger.
        (sample as i64).max(1)
    }
}

// ─── TaskDumped future wrapper ──────────────────────────────────────────────

pin_project! {
    /// Future wrapper that captures async backtraces at yield points using
    /// geometric (Poisson) sampling keyed on idle duration.
    pub(crate) struct TaskDumped<F> {
        #[pin]
        inner: F,
        shared: Arc<SharedState>,
        task_id: TaskId,
        frames: FrameBuf,
        // Monotonic nanoseconds when the frames in `frames` were captured.
        // Only meaningful when `frames.has_data()`.
        pending_capture_ts: Option<NonZeroU64>,
        // Geometric sampling state: remaining nanoseconds of idle time before
        // the next sample triggers. Signed so subtracting a large idle from a
        // small remaining value goes negative rather than wrapping.
        next_sample_ns: i64,
        // Mean of the exponential distribution (nanoseconds).
        sample_mean_ns: u64,
        // Per-task PRNG for drawing exponential gaps.
        rng: SplitMix64,
    }
}

impl<F> TaskDumped<F> {
    pub(crate) fn new(inner: F, shared: Arc<SharedState>, task_id: TaskId) -> Self {
        let sample_mean_ns = shared.task_dump_idle_threshold_ns.load(Ordering::Relaxed);
        // Seed the PRNG from the task ID + a timestamp for uniqueness across tasks.
        let seed = (task_id.to_u64()).wrapping_mul(0x517cc1b727220a95)
            ^ crate::telemetry::events::clock_monotonic_ns();
        let mut rng = SplitMix64::new(seed);
        let next_sample_ns = rng.draw_exponential_ns(sample_mean_ns);
        Self {
            inner,
            shared,
            task_id,
            frames: FrameBuf::new(),
            pending_capture_ts: None,
            next_sample_ns,
            sample_mean_ns,
            rng,
        }
    }
}

impl<F: Future> Future for TaskDumped<F> {
    type Output = F::Output;

    fn poll(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<F::Output> {
        let mut this = self.project();

        // Fast path: forward without any capture work when either task dumps
        // are disabled, or telemetry as a whole is paused.
        if !this.shared.task_dumps_enabled.load(Ordering::Relaxed) || !this.shared.is_enabled() {
            if this.frames.has_data() {
                this.frames.clear();
                *this.pending_capture_ts = None;
            }
            return this.inner.poll(cx);
        }

        // Geometric sampling: subtract the idle duration from the counter.
        // If it goes to zero or below, we should emit.
        let poll_start = crate::telemetry::recorder::poll_start_ts_or_now();
        let should_emit = match *this.pending_capture_ts {
            Some(ts) if this.frames.has_data() => {
                let idle_ns = poll_start.saturating_sub(ts.get()) as i64;
                *this.next_sample_ns -= idle_ns;
                *this.next_sample_ns <= 0
            }
            _ => false,
        };

        let result = this.inner.as_mut().poll(cx);

        if should_emit {
            this.frames.emit(
                this.shared,
                *this.task_id,
                this.pending_capture_ts.unwrap().get(),
            );
            // Redraw: loop in case the draw is smaller than the deficit
            // (rare, but possible for very large idles / small means).
            while *this.next_sample_ns <= 0 {
                *this.next_sample_ns += this.rng.draw_exponential_ns(*this.sample_mean_ns);
            }
        }

        match &result {
            Poll::Ready(_) => {
                this.frames.clear();
                *this.pending_capture_ts = None;
            }
            Poll::Pending => {
                this.frames.capture(this.inner.as_mut());
                *this.pending_capture_ts = NonZeroU64::new(poll_start);
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

/// Walk the stack, collecting instruction pointers between `leaf_addr` and
/// `root_addr`. Calls `_Unwind_Backtrace` directly via [`crate::unwind`],
/// bypassing the `backtrace` crate's process-wide mutex.
fn capture_frames(
    ips: &mut Vec<u64>,
    root_addr: Option<*const core::ffi::c_void>,
    leaf_addr: *const core::ffi::c_void,
) {
    crate::unwind::collect_frames(ips, root_addr, leaf_addr);
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
