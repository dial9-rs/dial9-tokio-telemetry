# In-memory pipeline

> **Status: design, not yet implemented.**

## Overview

Let dial9 run with no filesystem dependency. Users with plenty of RAM, or running in environments where disk is unavailable or unwelcome, get a memory-only path: encoded bytes flow from the writer through an in-process queue to the worker, then through the existing `SegmentProcessor` pipeline to whichever destination they configured.

The existing lifecycle is disk-mediated end to end. Flush thread writes encoded batches into `trace.N.bin.active`, writer renames to `trace.N.bin` on rotation, worker polls the directory every second, reads the file back into memory, and runs the processor pipeline. Removing the disk hop replaces the rename-then-rescan handoff with a bounded in-process queue and a wakeup primitive, while keeping the recorder hot path, the encoder, and the processor chain untouched.

**Core principle:** the disk path stays the default and unchanged. Memory mode is one writer swap on the existing runtime builder.

## Goals

- A memory-only path that never touches the filesystem for the segment data itself. Encoded bytes flow writer, in-process queue, worker, destination.
- Reuse the existing `SegmentProcessor` pipeline. No new API for users who already wire `.s3(...)` or custom processors.
- Bounded memory with the same "drop oldest, never block the recorder" policy the disk path already uses for eviction.

## Non-goals

- Recovering in-flight segments after a process crash. Anything in memory at crash time is gone. The disk path already drops segments under eviction pressure, 
so "data loss is sometimes okay" is an existing invariant.
- Spill-to-disk fallback when the memory budget is exhausted. A follow-up could make this configurable, but avoiding disk altogether keeps the feature usable in environments
where disk access is unavailable.
- Single-file mode analogue (`RotatingWriter::single_file`). Memory writer always rotates.

---

## 1. Architecture

```
Application threads                Worker thread (dedicated current_thread rt)
─────────────────────              ─────────────────────────────────────────
record_event() ──┐
                 ▼
          ThreadLocalBuffer
                 │ flush
                 ▼
        CentralCollector (existing)
                 │ drain
                 ▼
          MemoryWriter ─── queue+Notify ─▶ WorkerLoop
          (encode + seal)                  │
          drop-oldest if over              ▼
          max_buffered_bytes        Pipeline (SegmentProcessor chain):
                                      SymbolizeProcessor
                                      GzipCompressor
                                      S3PipelineUploader / user processor
```

The hot path (recording, thread-local flush, central collector) does not change. The work is on the writer (new `MemoryWriter`), the worker (new `SegmentSource` arm plus per-processor failure wrapping that benefits the disk path too), and the in-pipeline carrier (`SegmentRef` enum so processors operate in both modes).

### `MemoryWriter`

A new `TraceWriter` impl that lives next to `RotatingWriter`.

```rust
pub struct MemoryWriter {
    base: PathBuf,                          // virtual; used for stem / metadata
    rotation_period: Duration,
    next_rotation_time: SystemTime,
    drain_interval: Duration,
    next_drain_time: SystemTime,
    max_segment_size: u64,
    max_buffered_bytes: u64,

    state: WriterState,                     // Active { encoder, ... } | Finished
    next_index: u32,
    did_rotate: bool,
    segment_metadata: SegmentMetadata,
    has_real_events: bool,

    // Shared with the worker. See surrounding prose for semantics.
    queue: Arc<BoundedQueue<EncodedSegment>>,
    notify: Arc<Notify>,
    buffered_bytes: Arc<AtomicU64>,   // gauge only; not eviction trigger
    dropped_segments: Arc<AtomicU64>,
    writer_done: Arc<AtomicBool>,
}

/// Writer-to-worker queue carrier (`pub(crate)`). Mirrors `Batch`
/// shape. Worker wraps into `SegmentData` on pop.
pub(crate) struct EncodedSegment {
    pub payload: Payload,
    pub metadata: HashMap<String, String>,
    pub index: u32,
}

/// Consumer-side handle. Clones via Arc bumps. The writer hands one of
/// these to the runtime builder, which gives it to the worker. Bundles
/// the four shared primitives so we don't thread them through
/// `BackgroundTaskConfig` individually.
pub struct MemoryConsumer {
    queue: Arc<BoundedQueue<EncodedSegment>>,
    notify: Arc<Notify>,
    buffered_bytes: Arc<AtomicU64>,
    writer_done: Arc<AtomicBool>,
}

impl MemoryWriter {
    pub fn consumer(&self) -> MemoryConsumer { /* clone the four Arcs */ }
}
```

- The active segment is a `Vec<u8>` plus a `RawEncoder<Vec<u8>>` writing into it. The same `prepare_segment` / `write_metadata_if_needed` code as
`RotatingWriter` applies, parameterized over the underlying writer type.
- Rotation (time- or size-triggered, identical policy to the disk path) seals the active vec into a `Segment` and pushes it onto `queue`. A new
active vec is allocated for the next segment.
- Reuses the `should_drain` / `drained` / `take_rotated` semantics from the `TraceWriter` trait. In memory mode, rotation seals the active vec and pushes it onto the queue. Disk path unchanged.
- `finalize` pushes the active segment onto the queue, sets a "writer done" flag the worker observes, and notifies. The worker drains the
queue and exits.

The seal path:

```rust
fn seal_and_push(&mut self) -> std::io::Result<()> {
    let prev = std::mem::replace(&mut self.state, MemoryWriterState::Finished);
    let MemoryWriterState::ActivePlain { writer, .. } = prev else { return Ok(()) };

    let bytes = writer.into_inner();
    let len = bytes.len() as u64;
    let segment = EncodedSegment {
        payload: Payload::from_vec(bytes),
        metadata: self.segment_metadata().iter().cloned().collect(),
        index: self.next_index - 1,
    };

    self.buffered_bytes.fetch_add(len, AcqRel);
    if let Some(evicted) = self.queue.force_push(segment) {
        self.buffered_bytes.fetch_sub(evicted.payload.len() as u64, AcqRel);
        self.dropped_segments.fetch_add(1, Relaxed);
        // `evicted` drops at end of scope, freeing its `Bytes` Arcs.
    }
    self.notify.notify_one();

    let new_buf = Vec::with_capacity(self.max_segment_size as usize);
    self.state = MemoryWriterState::ActivePlain {
        writer: Encoder::new_to(new_buf)?.into_raw_encoder(),
        need_metadata: true,
    };
    Ok(())
}
```

There's no separate `evict_oldest` path. Eviction is whatever `force_push` returned.

Two cooperating primitives carry segments from writer to worker:

- **Storage:** `BoundedQueue<EncodedSegment>` from `crate::primitives` (lock-free `crossbeam_queue::ArrayQueue` in production, `Mutex<VecDeque>` under `--cfg shuttle`). Same primitive `CentralCollector` already uses for batches with the same drop-oldest discipline. Capacity is derived from the byte budget at builder time: `capacity = ceil(max_buffered_bytes / max_segment_size) + headroom`. The byte budget is the user-facing knob; capacity is internal.
- **Wakeup:** `tokio::sync::Notify`. Writer calls `notify_one()` after each push (sync, non-blocking). Worker awaits via `notify.notified().await` between drains. See the rejected-alternatives subsection below for the choice rationale and the new `primitives::sync::Notify` shuttle shim.

Queue traffic stays well below the recording hot path: pushes and pops happen at segment-seal rate (a few per second at most under bursty load, slower under steady load). Wakeups are at the same rate.

**Invariant: exactly one consumer.** `Notify` wakes one waiter per `notify_one`; a second worker would race on `notified()` and silently steal half the wakeups. If parallel processing is ever wanted (e.g. multiple S3 uploads in flight), it lives **inside** the sink (one worker, one sink, sink fans out internally), not as multiple workers reading the same queue.

### Channel-driven worker

The worker's existing poll-the-directory loop becomes one variant of a
`SegmentSource`:

```rust
pub(crate) enum SegmentSource {
    Disk {
        dir: PathBuf,
        stem: String,
        poll_interval: Duration,
    },
    Channel {
        queue: Arc<BoundedQueue<EncodedSegment>>,
        notify: Arc<Notify>,
        buffered_bytes: Arc<AtomicU64>,
        writer_done: Arc<AtomicBool>,
    },
}

impl SegmentSource {
    async fn next(&mut self, stop: &CancellationToken) -> Option<SegmentData> {
        match self {
            Self::Disk { dir, stem, poll_interval } => { /* existing logic */ }
            Self::Channel { queue, notify, buffered_bytes, writer_done } => {
                loop {
                    // Register interest before checking queue: any later
                    // `notify_one` becomes the permit this future consumes.
                    let notified = notify.notified();
                    tokio::pin!(notified);
                    notified.as_mut().enable();

                    if let Some(seg) = queue.pop() {
                        return Some(SegmentData::from_encoded(seg));
                    }
                    // Re-pop after observing a shutdown signal: writer
                    // may have pushed between our pop and this load.
                    if stop.is_cancelled() || writer_done.load(Acquire) {
                        return queue.pop().map(SegmentData::from_encoded);
                    }
                    notified.await;
                }
            }
        }
    }
}
```

The worker grows a `SegmentSource::Channel` arm alongside the existing `Disk` arm. The channel arm pops from the queue, wraps each `EncodedSegment` into a `SegmentData`, and runs the same processor chain as the disk arm. Per-processor failure wrapping (section 2) lives in a shared helper used by both arms.

### Rejected alternatives

**Queue storage.**

- *`Mutex<VecDeque<EncodedSegment>>`*: works, supports head-removal from both producer (eviction) and consumer. But adds a sync mutex on a path the rest of the crate keeps lock-free for shared queues; doesn't match the dial9 convention.
- *`crossbeam_queue::SegQueue`*: unbounded, no eviction primitive. Doesn't model drop-oldest.
- *`tokio::sync::mpsc::Receiver` (segments-as-payload)*: has built-in wakeup but no eviction; would need a side channel.
- *Hand-rolled ring buffer*: unnecessary given we can use `BoundedQueue`.

**Wakeup primitive.**

- *Sync `mpsc<()>(1)` (`crate::primitives::sync::mpsc`)*: already shuttle-shimmed, but `Receiver::recv()` is blocking and the worker runs on a `new_current_thread` runtime, so blocking it 
stalls the entire worker (including in-flight sink futures, timeouts, cancellation). Would force a `spawn_blocking` bridge per wait, meaning thread-pool churn plus scheduler hop for a wakeup that fires once per segment.
- *`tokio::sync::mpsc::channel::<()>(1)`*: async-native but semantically a value queue used for unit signals. Larger surface to shuttle-shim than `Notify`.
- *Poll the queue on a `tokio::time::sleep` interval*: works, no new primitive, but loses event-driven latency (segments wait up to `poll_interval` after seal).

**Writer-to-worker handoff unit.**

- *Continuous byte stream*: worker consumes a stream with no segment boundary. Doesn't match per-segment metrics, S3 multipart upload, retry semantics, or the existing rotation contract. 
Also forces every downstream processor to handle partial / out-of-order bytes.

---

## 2. Pipeline integration

Memory mode reuses the existing pipeline.

The only obstacle is `SegmentData::segment`, today filesystem-coupled (`SealedSegment { path, index }`). Memory mode has no path. We generalize via a sibling type plus enum:

```rust
pub struct MemorySegment { pub index: u32 }

pub enum SegmentRef {
    Disk(SealedSegment),
    Memory(MemorySegment),
}
```

`SegmentData::segment()` returns `&SegmentRef`. Existing disk-only processors (`WriteBackProcessor`, the path-logging branch of `S3PipelineUploader`) `match` on the enum. `WriteBackProcessor` paired with a memory writer is rejected by the runtime builder at `build()` time, so this is defense in depth rather than a runtime expectation.

> **Breaking change.** `SegmentData::segment()` return type goes from `&SealedSegment` to `&SegmentRef`. External `SegmentProcessor` impls that called `data.segment().path()` need to match on the enum. Internal processors are updated in this change.

### Failure modes: timeout, retry budget, panic

Apply to every pipeline stage (memory or disk). Worker wraps each processor's `process(...)` future:

- **Per-attempt timeout.** `tokio::time::timeout(attempt_timeout, fut)`. Timeout → `ProcessErrorKind::Transfer { retryable: true }`. Default 30 s; configurable per processor.
- **Bounded retry budget.** On `retryable: true`, the worker re-runs the stage up to `retry_budget` times before dropping the segment. Default 3. `S3PipelineUploader` already wraps its own
`CircuitBreaker`, so it's instantiated with `retry_budget = 1`.
- **Panic isolation.** Stage future runs inside `AssertUnwindSafe(...).catch_unwind()`. Panic → segment dropped, metrics fire, stage instance kept (so transient state like connection pools survives). **Contract for custom processor authors:** a panicked `process(...)` must leave the instance valid for the next call (no held locks, no half-filled buffers, no corrupted state).

These attach to processors via a `PipelineBuilder::pipe_with_config` variant (or equivalent); built-ins set sane defaults.

---

## 3. Memory story

### What gets allocated

Three byte pools contribute to peak working set:

| Bucket | Owner | Size | Notes |
|--------|-------|------|-------|
| Active segment buffer | flush thread | up to `max_segment_size` (user-set) | `Vec<u8>` the encoder writes into; `RawEncoder` is a thin wrapper, no extra overhead |
| Queued segments | shared queue | up to `max_buffered_bytes` minus active + in-flight | Sealed `EncodedSegment`; `Payload::from_vec` wraps zero-copy |
| In-flight pipeline data | worker | 1 segment + gzip output | `SymbolizeProcessor` (cpu-profiling only) appends ~5–20% symbol-table chunk; `GzipCompressor` output is ~25–35% of input |

Symbol-table and gzip-ratio numbers are placeholders pending the measurement bench.

Note on interning: per-batch string interning lives on the recording thread, not the writer. Each `ThreadLocalBuffer` resets its encoder on every flush (`Encoder::reset_to_infallible`), so the interner peak is bounded by one batch encode, not a segment's life. The writer's own `Encoder` (for segment-header metadata) is small and fixed.

### Comparison to the disk path

The disk path already pays for the active buffer and the in-flight pipeline data: `BufWriter<File>` holds an 8 KB write buffer plus the active file's data in OS page cache, and the worker reads each segment back into a `Vec<u8>` to run the pipeline. So for any disk-backed run, peak per-segment working set is already ~`max_segment_size` + gzip output, the same as the memory path.

What memory mode adds relative to disk is the queued segments: they live in process heap instead of as `.bin` files. For the same total budget, disk lets the OS swap or page out cold pages under pressure; memory mode keeps them pinned in the process. Services with strict working-set ceilings will care. Eviction policy is the same (drop oldest, never block), fires under the same "worker can't keep up" trigger, so the drop window size is identical for a given budget.

`max_buffered_bytes` is a required builder argument. This matches `RotatingWriter::new`'s `max_total_size` / `max_file_size` pattern: storage budgets are environment-specific. The memory path also skips the OS page cache that disk leans on for burst headroom, so the picked number has to absorb bursts on its own.

For a service producing one segment per minute with `max_buffered_bytes = 16 MB` and `max_segment_size = 1 MB`, peak working set is roughly `1 MB (active) + 1 MB (in-flight pipeline) + min(queued, 14 MB)`, which is ~16 MB worst case and ~3 MB at steady state when the worker keeps up.

### Byte accounting and drop-oldest

Eviction discipline lives in the queue itself: `BoundedQueue::force_push` drops the oldest segment when the ring is full. Capacity is sized at builder time from `max_buffered_bytes / max_segment_size + headroom`, so the effective byte cap is `capacity × max_segment_size` (slightly looser than the configured budget when segments are smaller than the max, never tighter).

`buffered_bytes: Arc<AtomicU64>` is a gauge over **live encoded bytes** (queued + in-flight). It is observability and diagnostics, not the eviction trigger.


| Event                                                  | `buffered_bytes` change                                 | Notes                                        |
| ------------------------------------------------------ | ------------------------------------------------------- | -------------------------------------------- |
| Active buffer fills, encoder grows                     | unchanged                                               | bytes still owned by encoder; not yet sealed |
| `seal_and_push` → `force_push` accepts (ring not full) | `+= segment_size`                                       | segment in ring                              |
| `seal_and_push` → `force_push` evicts                  | `+= new_size, -= evicted_size`, `dropped_segments += 1` | net delta depends on sizes                   |
| Worker pops + pipeline runs                            | unchanged                                               | in-flight, dropped on `SegmentData` drop     |
| `SegmentData` drops at pipeline end                    | `-= segment_size`                                       | accounting closes                            |


The decrement at `SegmentData` drop runs regardless of pipeline outcome (success, retry-exhaustion, panic). In-flight segments are out of the ring once popped, so they can't be evicted again; they ride to completion and decrement on drop.

Stage-internal allocations (`SymbolizeProcessor` appending a symbol table, `GzipCompressor` flattening + compressing) don't update `buffered_bytes`. The gauge tracks writer-emitted bytes; transient pipeline growth dies with `SegmentData` drop.

Drop signals: `dropped_segments` counter increments per evicted segment (see section 6 metrics catalog), plus a rate-limited `tracing::warn!` so sustained overruns are visible in logs.

---

## 4. API surface

User passes a `MemoryWriter` into `build_and_start_with_writer` the same way they'd pass a `RotatingWriter`. The runtime builder pulls the consumer-side handle out of the writer via a new trait method on `TraceWriter`:

```rust
pub trait TraceWriter: Send {
    // existing methods...

    /// In-memory writers expose a consumer handle so the worker can pop
    /// sealed segments from the shared queue. Default returns None; only
    /// memory-mode writers override.
    fn consumer(&self) -> Option<MemoryConsumer> { None }
}
```

`MemoryWriter::consumer` overrides to return `Some(...)`. The runtime builder calls this before moving the writer into the flush thread; if `Some`, it wires the worker's `SegmentSource::Channel` arm.

```rust
// Memory path, S3 destination
let writer = MemoryWriter::builder()
    .max_buffered_bytes(16 * MB)        // required
    .max_segment_size(1 * MB)           // required (mirrors RotatingWriter)
    .build()?;
let (runtime, guard) = TracedRuntime::builder()
    .with_s3_uploader(s3_config)
    .build_and_start_with_writer(tokio_builder, writer)?;
```

For custom destinations (GCS, HTTP, etc.) swap `.with_s3_uploader(...)` for `.with_custom_pipeline(|p| p.pipe(MyProcessor).gzip())`. Same shape.

`MemoryWriter` builder defaults:

- `rotation_period`: reuses disk's `DEFAULT_ROTATION_PERIOD` (60 s). Caps how long an event can sit in the active buffer when traffic is low.
- `drain_interval`: internal, not user-facing.
- `queue_capacity`: derived from `max_buffered_bytes / max_segment_size + headroom` (e.g. 16 / 1 + 2 = 18 slots for the example above). User passes bytes, the builder translates to a slot count for `BoundedQueue`.

`BackgroundTaskConfig` itself stays internal; users compose via the runtime builder.

**Alternative considered:** A two-step API where the user calls `let consumer = writer.consumer();` and passes the consumer separately via `.with_memory_consumer(consumer)` would also work, but it leaves the door open for passing a consumer and writer from different instances (compiles, fails at runtime). The trait-method approach keeps the consumer pinned to its writer.

---

## 5. Crash and shutdown semantics

Memory path: anything in the channel or in `SegmentData` at crash time is gone.

Graceful shutdown for memory mode mirrors disk:

1. Stop the flush thread.
2. `MemoryWriter::finalize`: seal the active segment, push it to the queue, set `writer_done` (`AtomicBool`, `Release`) so the worker's final empty-queue check sees the writer as done, and call
  `notify.notify_one()` to wake the worker. Then drop the writer's handles to the queue.
3. Worker drains the queue and runs each segment through the pipeline.
4. `TelemetryGuard::graceful_shutdown(timeout)` waits for the worker to exit.

---

## 6. Testing

Reuse the patterns from the disk path:

- **Unit.** `MemoryWriter` rotation, eviction-on-budget-overrun, segment count matches expected, `finalize` drains the active segment.
- **Integration.** Pair `MemoryWriter` with a test `SegmentProcessor` that captures segments into a `Vec<SegmentData>`. Run a workload, confirm bytes decode, confirm event counts match.
- **S3 integration.** `MemoryWriter` + `S3PipelineUploader` against `s3s` (already used by the disk-path S3 tests). Same assertions.
- **Stress.** High segment-emission rate against a slow downstream processor. Verify drop-oldest fires, counters increment, the writer never blocks.
- **Streaming compression.** Golden-file decompression: write a known payload streamed, confirm gunzip output equals the un-streamed payload. Plus a compression-ratio sanity check (within ~5% of batched).
- **Shuttle.** `BoundedQueue`, atomics, and the `Notify` wakeup go through `crate::primitives::sync` so `--cfg shuttle` builds reuse the existing shim plus the small `primitives::sync::Notify` shim added for this design. Scenarios worth modeling:
  - Concurrent `force_push` (writer) and `pop` (worker) on the same `BoundedQueue` instance.
  - Lost-wakeup avoidance
  - Shutdown race
  - Eviction under contention (writer's `force_push` returns `Some(evicted)` while worker is mid-pipeline on a separately-popped segment; verify `buffered_bytes` and `dropped_segments` remain consistent across interleavings)

### Memory regression tests

Memory mode keeps bytes in process heap that the disk path kept on disk, so regressions are easier to introduce and harder to spot. Two layers, both run per-PR:

- **Counter assertions.** Fixed-seed workload against `MemoryWriter`. Assert peak `buffered_bytes` stays within the configured budget, eviction fires when the budget is intentionally tight, queue depth stays bounded. Catches accounting regressions.
- **Heap baseline.** Same workload under a deterministic heap profiler (e.g. `dhat`), baseline checked into CI. Catches leaks and allocator-side regressions the counters can't see.

For diagnosis when a gate trips, the upcoming heap profiler (`design/memory-profiling.md`) could be very useful to find where the bytes came from.

### Metrics catalog

Reuses the existing `SegmentProcessMetrics` + per-stage prefix infrastructure (`src/metrics.rs`, `src/background_task/pipeline_metrics.rs`). Existing per-segment fields (`TotalTime`, `{Stage}.Time`, `Panicked`, etc.) are unchanged; see those modules for the authoritative list. This PR adds:

**Memory-mode additions.**


| Metric                        | Type    | Description                                                                                                                    |
| ----------------------------- | ------- | ------------------------------------------------------------------------------------------------------------------------------ |
| `BufferedBytes`               | gauge   | Current `buffered_bytes` (active + queued + in-flight)                                                                         |
| `QueueDepth`                  | gauge   | segments currently in the ring (requires adding `BoundedQueue::len()`; `ArrayQueue::len` already exists, just not yet exposed) |
| `DroppedSegments`             | counter | Increments on each `force_push` eviction                                                                                       |
| `MemoryWriter.SealedSegments` | counter | Successful seal+push events                                                                                                    |


**Per-stage failure-mode additions** (`{Stage}` is the processor's `name()`):


| Metric                    | Type    | Description                                              |
| ------------------------- | ------- | -------------------------------------------------------- |
| `{Stage}.RetryableErrors` | counter | `ProcessErrorKind::Transfer { retryable: true }` returns |
| `{Stage}.PoisonDrops`     | counter | Segments dropped after `retry_budget` exhaustion         |
| `{Stage}.Timeouts`        | counter | `attempt_timeout` fires                                  |


`BufferedBytes` and `QueueDepth` sample on a low-frequency timer (default 1 s) to avoid hot-path contention; `DroppedSegments` and the per-stage counters are event-driven.

---

## 7. Open questions

**Make `trace_path` / `trace_dir` optional?** Memory mode doesn't use the trace path. Cleanup: turn `BackgroundTaskConfig::trace_path` into `Option<PathBuf>`. Easy non-breaking approach is to keep `trace_dir()` / `trace_stem()` returning `&Path` / `&str` with their existing fallback defaults when the path is missing.

**Disk-vs-memory mutex at the type level.** In this doc the precedence is runtime (memory wins, section 4). We could split the marker chain so calling both is a compile error, but it would double the `(path-state, pipeline-state)` matrix for one mutually-exclusive pair.

**Adaptive sizing:** The current design takes a static `max_buffered_bytes`. We could measure queue depth and let the writer grow/shrink within a configured range, similar to how some allocators auto-tune their arenas. Perhaps container users could benefit more from reading cgroup `memory.max` / `memory.high` as an outer cap. Initially keeping it out of scope for v1 or at least until implementation testing shows workload data. The metrics catalog (`BufferedBytes`, `QueueDepth`, `DroppedSegments`) covers the internal inputs we'd need.
