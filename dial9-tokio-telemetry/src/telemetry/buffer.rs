//! `ThreadLocalBuffer` is the entrypoint for almost all dial9 events
//!
//! The TL buffer is created lazily the first time an event is sent. Events are encoded directly
//! into a thread-local `Encoder<Vec<u8>>` and flushed to the central collector when the encoded
//! batch reaches the configured batch size (default 1 MB).
//!
//! Each buffer is wrapped in `Arc<Mutex<…>>` so the flush thread can intrusively
//! drain idle/silent threads via [`TlBufferHandle`]s registered in `SharedState`.
use crate::telemetry::collector::CentralCollector;
use crate::telemetry::events::RawEvent;
use crate::telemetry::format::*;
use dial9_trace_format::InternedString;
use dial9_trace_format::encoder::Encoder;
use std::collections::HashMap;
use std::panic::Location;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

// ── Public API types ────────────────────────────────────────────────────────

/// Scoped encoder for writing events into the thread-local trace buffer.
///
/// Provides access to string interning and event encoding. The borrow lifetime
/// ensures that [`InternedString`] handles created via [`intern_string`](Self::intern_string)
/// are used within the same batch — they become invalid after the buffer flushes.
///
/// You don't construct this directly; it's passed to [`Encodable::encode`].
pub struct ThreadLocalEncoder<'a> {
    encoder: &'a mut Encoder<Vec<u8>>,
    location_cache: &'a mut HashMap<&'static Location<'static>, String>,
}

impl std::fmt::Debug for ThreadLocalEncoder<'_> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("ThreadLocalEncoder").finish_non_exhaustive()
    }
}

impl ThreadLocalEncoder<'_> {
    /// Intern a string into the trace's string pool, returning a compact handle.
    ///
    /// If the string was already interned in this batch, returns the existing handle
    /// (no duplicate wire data). The returned [`InternedString`] is only valid for
    /// encoding within this same [`Encodable::encode`] call.
    pub fn intern_string(&mut self, s: &str) -> InternedString {
        self.encoder.intern_string_infallible(s)
    }

    /// Encode a [`TraceEvent`](dial9_trace_format::TraceEvent) struct into the buffer.
    ///
    /// The `'static` bound is required because the encoder uses [`TypeId`](std::any::TypeId)
    /// to cache schema registrations per concrete type.
    pub fn encode(&mut self, event: &(impl dial9_trace_format::TraceEvent + 'static)) {
        self.encoder.write_infallible(event);
    }

    /// Intern a `&'static Location` (caching the `to_string()` result).
    pub(crate) fn intern_location(
        &mut self,
        location: &'static Location<'static>,
    ) -> InternedString {
        let s = self
            .location_cache
            .entry(location)
            .or_insert_with(|| location.to_string());
        self.encoder.intern_string_infallible(s)
    }
}

/// Trait for types that can be encoded into a dial9 trace.
///
/// # Simple case — `#[derive(TraceEvent)]`
///
/// Any type implementing [`TraceEvent`](dial9_trace_format::TraceEvent) automatically
/// implements `Encodable` via a blanket impl, so you can pass it directly to
/// [`record_event`](crate::telemetry::record_event):
///
/// ```ignore
/// #[derive(TraceEvent)]
/// struct MyEvent {
///     #[traceevent(timestamp)]
///     timestamp_ns: u64,
///     request_count: u32,
/// }
/// record_event(MyEvent { timestamp_ns: now, request_count: 42 }, &handle);
/// ```
///
/// # Advanced case — string interning
///
/// Implement `Encodable` manually when you need [`InternedString`] fields
/// for efficient repeated-string encoding:
///
/// ```ignore
/// struct HttpRequest { timestamp_ns: u64, method: String, status: u32 }
///
/// impl Encodable for HttpRequest {
///     fn encode(&self, enc: &mut ThreadLocalEncoder<'_>) {
///         let method = enc.intern_string(&self.method);
///         enc.encode(&HttpRequestWire {
///             timestamp_ns: self.timestamp_ns,
///             method,
///             status: self.status,
///         });
///     }
/// }
/// ```
///
/// # Wire event naming
///
/// The event name in the trace comes from the struct passed to
/// [`ThreadLocalEncoder::encode`], not from the type implementing `Encodable`.
/// In the example above, the trace will contain events named `"HttpRequestWire"`,
/// not `"HttpRequest"`.
pub trait Encodable {
    /// Encode this event into the thread-local trace buffer.
    ///
    /// Implementations should call [`ThreadLocalEncoder::encode`] exactly once.
    /// Each `encode` call is counted as one event for buffer flush decisions;
    /// calling `encode` multiple times will produce multiple wire events but
    /// only one event will be counted.
    fn encode(&self, encoder: &mut ThreadLocalEncoder<'_>);
}

impl<T: dial9_trace_format::TraceEvent + 'static> Encodable for T {
    fn encode(&self, encoder: &mut ThreadLocalEncoder<'_>) {
        encoder.encode(self);
    }
}

impl Encodable for RawEvent {
    fn encode(&self, enc: &mut ThreadLocalEncoder<'_>) {
        match self {
            RawEvent::PollStart {
                timestamp_nanos,
                worker_id,
                worker_local_queue_depth,
                task_id,
                location,
            } => {
                let spawn_loc = enc.intern_location(location);
                enc.encode(&PollStartEvent {
                    timestamp_ns: *timestamp_nanos,
                    worker_id: *worker_id,
                    local_queue: *worker_local_queue_depth as u8,
                    task_id: *task_id,
                    spawn_loc,
                });
            }
            RawEvent::PollEnd {
                timestamp_nanos,
                worker_id,
            } => enc.encode(&PollEndEvent {
                timestamp_ns: *timestamp_nanos,
                worker_id: *worker_id,
            }),
            RawEvent::WorkerPark {
                timestamp_nanos,
                worker_id,
                worker_local_queue_depth,
                cpu_time_nanos,
            } => enc.encode(&WorkerParkEvent {
                timestamp_ns: *timestamp_nanos,
                worker_id: *worker_id,
                local_queue: *worker_local_queue_depth as u8,
                cpu_time_ns: *cpu_time_nanos,
            }),
            RawEvent::WorkerUnpark {
                timestamp_nanos,
                worker_id,
                worker_local_queue_depth,
                cpu_time_nanos,
                sched_wait_delta_nanos,
            } => enc.encode(&WorkerUnparkEvent {
                timestamp_ns: *timestamp_nanos,
                worker_id: *worker_id,
                local_queue: *worker_local_queue_depth as u8,
                cpu_time_ns: *cpu_time_nanos,
                sched_wait_ns: *sched_wait_delta_nanos,
            }),
            RawEvent::QueueSample {
                timestamp_nanos,
                global_queue_depth,
            } => enc.encode(&QueueSampleEvent {
                timestamp_ns: *timestamp_nanos,
                global_queue: *global_queue_depth as u8,
            }),
            RawEvent::TaskSpawn {
                timestamp_nanos,
                task_id,
                location,
            } => {
                let spawn_loc = enc.intern_location(location);
                enc.encode(&TaskSpawnEvent {
                    timestamp_ns: *timestamp_nanos,
                    task_id: *task_id,
                    spawn_loc,
                });
            }
            RawEvent::TaskTerminate {
                timestamp_nanos,
                task_id,
            } => enc.encode(&TaskTerminateEvent {
                timestamp_ns: *timestamp_nanos,
                task_id: *task_id,
            }),
            RawEvent::WakeEvent {
                timestamp_nanos,
                waker_task_id,
                woken_task_id,
                target_worker,
            } => enc.encode(&WakeEventEvent {
                timestamp_ns: *timestamp_nanos,
                waker_task_id: *waker_task_id,
                woken_task_id: *woken_task_id,
                target_worker: *target_worker,
            }),
            RawEvent::CpuSample(data) => {
                let thread_name = match &data.thread_name {
                    Some(name) => enc.intern_string(name.as_str()),
                    None => enc.intern_string("<no thread name>"),
                };
                enc.encode(&CpuSampleEvent {
                    timestamp_ns: data.timestamp_nanos,
                    worker_id: data.worker_id,
                    tid: data.tid,
                    source: data.source,
                    thread_name,
                    callchain: dial9_trace_format::StackFrames(data.callchain.clone()),
                });
            }
        }
    }
}

// ── Thread-local buffer internals ───────────────────────────────────────────

/// Tracks the last drain epoch at which a particular thread-local buffer
/// was flushed. The flush thread reads this (relaxed) to skip buffers
/// that have self-flushed recently, avoiding contention with busy workers.
#[derive(Clone)]
pub(crate) struct FlushEpoch(Arc<AtomicU64>);

impl FlushEpoch {
    pub(crate) fn new() -> Self {
        Self(Arc::new(AtomicU64::new(0)))
    }

    pub(crate) fn store(&self, epoch: u64) {
        self.0.store(epoch, Ordering::Relaxed);
    }

    pub(crate) fn load(&self) -> u64 {
        self.0.load(Ordering::Relaxed)
    }
}

/// Default maximum encoded batch size before flushing (~1MB).
const DEFAULT_BATCH_SIZE: usize = 1023 * 1024;

pub(crate) struct ThreadLocalBuffer {
    encoder: Encoder<Vec<u8>>,
    event_count: usize,
    batch_size: usize,
    collector: Option<Arc<CentralCollector>>,
    /// Caches `Location::to_string()` to avoid re-formatting on every event.
    /// Bounded by the number of `#[track_caller]` call sites in the program,
    /// which is fixed at compile time, so this does not grow unboundedly.
    location_cache: HashMap<&'static Location<'static>, String>,
    /// Last drain epoch at which this buffer was flushed. Shared with the
    /// flush thread via `TlBufferHandle` so it can skip busy workers.
    pub(crate) flush_epoch: FlushEpoch,
}

impl Default for ThreadLocalBuffer {
    fn default() -> Self {
        Self::new()
    }
}

impl ThreadLocalBuffer {
    fn new() -> Self {
        Self::with_batch_size(DEFAULT_BATCH_SIZE)
    }

    fn with_batch_size(batch_size: usize) -> Self {
        Self {
            // Allocate 1KB extra headroom so typical events never trigger a realloc.
            encoder: Encoder::new_to(Vec::with_capacity(batch_size + 1024))
                .expect("Vec::write_all cannot fail"),
            event_count: 0,
            batch_size,
            collector: None,
            location_cache: HashMap::new(),
            flush_epoch: FlushEpoch::new(),
        }
    }

    /// Ensure the collector reference is set. Called on every record_event;
    /// only the first call per thread actually stores the Arc.
    /// Returns `true` on the first call (when the collector was not yet set).
    fn set_collector(&mut self, collector: &Arc<CentralCollector>) -> bool {
        if self.collector.is_none() {
            self.collector = Some(Arc::clone(collector));
            return true;
        }
        false
    }

    fn thread_local_encoder(&mut self) -> ThreadLocalEncoder<'_> {
        ThreadLocalEncoder {
            encoder: &mut self.encoder,
            location_cache: &mut self.location_cache,
        }
    }

    fn record_encodable(&mut self, event: &dyn Encodable) {
        event.encode(&mut self.thread_local_encoder());
        self.event_count += 1;
    }

    /// Encode a single event into a self-contained batch (header + event).
    /// Used by tests that need to write individual events through the batch API.
    #[cfg(test)]
    pub(crate) fn encode_single(event: &RawEvent) -> Vec<u8> {
        let mut buf = Self::with_batch_size(1024);
        buf.record_encodable(event);
        buf.flush().encoded_bytes
    }

    fn should_flush(&self) -> bool {
        self.encoder.bytes_written() as usize >= self.batch_size
    }

    pub(crate) fn flush(&mut self) -> crate::telemetry::collector::Batch {
        let event_count = self.event_count as u64;
        let encoded_bytes = self
            .encoder
            .reset_to_infallible(Vec::with_capacity(self.batch_size));
        self.event_count = 0;
        crate::telemetry::collector::Batch {
            encoded_bytes,
            event_count,
        }
    }

    pub(crate) fn has_pending_events(&self) -> bool {
        self.event_count > 0
    }
}

impl Drop for ThreadLocalBuffer {
    fn drop(&mut self) {
        if self.event_count > 0 {
            if let Some(collector) = self.collector.take() {
                collector.accept_flush(self.flush());
            } else {
                crate::rate_limit::rate_limited!(Duration::from_secs(60), {
                    tracing::warn!(
                        "dial9-tokio-telemetry: dropping {} unflushed events (no collector registered on this thread)",
                        self.event_count
                    );
                });
            }
        }
    }
}

/// A handle to a thread-local buffer, held by `SharedState` so the flush
/// thread can intrusively drain idle/silent buffers.
pub(crate) struct TlBufferHandle {
    pub(crate) buffer: std::sync::Weak<Mutex<ThreadLocalBuffer>>,
    pub(crate) flush_epoch: FlushEpoch,
}

thread_local! {
    static BUFFER: Arc<Mutex<ThreadLocalBuffer>> = Arc::new(Mutex::new(ThreadLocalBuffer::new()));
}

/// Record an event into the current thread's buffer. If the buffer is full,
/// automatically flush the batch to `collector` and stamp the current
/// `drain_epoch` on the buffer's `FlushEpoch`.
///
/// On the first call per thread (when the collector is set), returns a
/// [`TlBufferHandle`] that the caller should register in `SharedState`
/// so the flush thread can drain this buffer.
pub(crate) fn record_event(
    event: RawEvent,
    collector: &Arc<CentralCollector>,
    drain_epoch: &AtomicU64,
) -> Option<TlBufferHandle> {
    record_encodable_event(&event, collector, drain_epoch)
}

/// Drain the current thread's buffer into `collector`, even if not full.
/// Used at shutdown and before flush cycles to avoid losing events.
pub(crate) fn drain_to_collector(collector: &CentralCollector) {
    BUFFER.with(|buf| {
        let mut buf = match buf.lock() {
            Ok(guard) => guard,
            Err(_) => {
                crate::rate_limit::rate_limited!(Duration::from_secs(60), {
                    tracing::error!("dial9: thread-local buffer mutex poisoned in drain_to_collector; skipping drain");
                });
                return;
            }
        };
        if buf.event_count > 0 {
            collector.accept_flush(buf.flush());
        }
    });
}

/// Record a user-defined event into the thread-local trace buffer.
///
/// Like [`record_event`] but accepts any [`Encodable`] type, including
/// user-defined `#[derive(TraceEvent)]` structs.
pub(crate) fn record_encodable_event(
    event: &dyn Encodable,
    collector: &Arc<CentralCollector>,
    drain_epoch: &AtomicU64,
) -> Option<TlBufferHandle> {
    BUFFER.with(|arc| {
        let mut buf = match arc.lock() {
            Ok(guard) => guard,
            Err(_) => {
                crate::rate_limit::rate_limited!(Duration::from_secs(60), {
                    tracing::error!("dial9: thread-local buffer mutex poisoned in record_encodable_event; dropping events for this thread");
                });
                return None;
            }
        };
        let first_call = buf.set_collector(collector);
        buf.record_encodable(event);
        let current_epoch = drain_epoch.load(Ordering::Relaxed);
        if buf.should_flush() || buf.flush_epoch.load() < current_epoch {
            collector.accept_flush(buf.flush());
            buf.flush_epoch.store(current_epoch);
        }
        if first_call {
            Some(TlBufferHandle {
                buffer: Arc::downgrade(arc),
                flush_epoch: buf.flush_epoch.clone(),
            })
        } else {
            None
        }
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    fn poll_end_event() -> RawEvent {
        RawEvent::PollEnd {
            timestamp_nanos: 1000,
            worker_id: crate::telemetry::format::WorkerId::from(0usize),
        }
    }

    #[test]
    fn test_buffer_creation() {
        let buffer = ThreadLocalBuffer::new();
        assert_eq!(buffer.event_count, 0);
        assert_eq!(buffer.batch_size, DEFAULT_BATCH_SIZE);
    }

    #[test]
    fn test_record_event() {
        let mut buffer = ThreadLocalBuffer::new();
        buffer.record_encodable(&poll_end_event());
        assert_eq!(buffer.event_count, 1);
        assert!(buffer.encoder.bytes_written() > 0);
    }

    #[test]
    fn test_should_flush_respects_batch_size() {
        // Use a tiny batch size so a single event triggers flush.
        let mut buffer = ThreadLocalBuffer::with_batch_size(1);
        assert!(!buffer.should_flush());
        buffer.record_encodable(&poll_end_event());
        assert!(buffer.should_flush());
    }

    #[test]
    fn test_should_flush_default_batch_size() {
        let mut buffer = ThreadLocalBuffer::new();
        assert!(!buffer.should_flush());
        buffer.record_encodable(&poll_end_event());
        // A single small event should not exceed 1 MB.
        assert!(!buffer.should_flush());
    }

    #[test]
    fn test_flush() {
        let mut buffer = ThreadLocalBuffer::new();
        buffer.record_encodable(&poll_end_event());
        let batch = buffer.flush();
        assert!(!batch.encoded_bytes.is_empty());
        assert_eq!(buffer.event_count, 0);
    }

    #[test]
    fn test_flush_epoch_store_load() {
        let epoch = FlushEpoch::new();
        assert_eq!(epoch.load(), 0);
        epoch.store(42);
        assert_eq!(epoch.load(), 42);
    }

    #[test]
    fn test_flush_epoch_shared_across_threads() {
        let epoch = FlushEpoch::new();
        let epoch_clone = epoch.clone();
        let handle = std::thread::spawn(move || {
            epoch_clone.store(7);
        });
        handle.join().unwrap();
        assert_eq!(epoch.load(), 7);
    }

    #[test]
    fn test_flush_epoch_stamped_on_self_flush() {
        let collector = Arc::new(CentralCollector::new());
        let drain_epoch = AtomicU64::new(5);
        // Use a tiny batch size so a single event triggers self-flush.
        // We can't use record_event (thread-local) easily, so test the
        // logic directly: flush + stamp.
        let mut buffer = ThreadLocalBuffer::with_batch_size(1);
        buffer.set_collector(&collector);
        buffer.record_encodable(&poll_end_event());
        assert!(buffer.should_flush());
        buffer
            .flush_epoch
            .store(drain_epoch.load(Ordering::Relaxed));
        collector.accept_flush(buffer.flush());
        assert_eq!(buffer.flush_epoch.load(), 5);
    }

    #[test]
    fn test_mutex_accessible_from_another_thread() {
        let buf = Arc::new(Mutex::new(ThreadLocalBuffer::new()));
        let buf_clone = Arc::clone(&buf);
        // Write an event from a different thread.
        let handle = std::thread::spawn(move || {
            let mut guard = buf_clone.lock().unwrap();
            guard.record_encodable(&poll_end_event());
            assert_eq!(guard.event_count, 1);
        });
        handle.join().unwrap();
        // Main thread can also access it.
        let guard = buf.lock().unwrap();
        assert_eq!(guard.event_count, 1);
    }
}
