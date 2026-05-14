# In-memory pipeline

> **Status: design, not yet implemented.**

## Overview

Let dial9 run with no filesystem dependency. Users with plenty of RAM, or running in environments where disk is unavailable or unwelcome, get a memory-only path: encoded bytes flow from the writer through an in-process queue to the worker, then through the existing `SegmentProcessor` pipeline to whichever destination they configured.

The existing lifecycle is disk-mediated end to end. Flush thread writes encoded batches into `trace.N.bin.active`, writer renames to `trace.N.bin` on rotation, worker polls the directory every second, reads the file back into memory and runs the processor pipeline.

A single `Fs` trait covers the writer + worker boundary. `DiskFs` wraps `std::fs` for the rotating disk path. `MemFs` keeps active bytes in a per-path `Vec<u8>` map and routes sealed bytes through an internal channel to the worker. The trait covers the full segment lifecycle: write-side (`create`, `seal`, `remove_*`) and read-side (`take_files`, `wait_for_more`, `writer_done`, `mark_writer_done`, `record_writer_eviction`).

**Core principle:** the disk path stays the default and unchanged. Memory mode is one constructor swap on the existing writer builder.

## Goals

- A memory-only path that never touches the real filesystem for the segment data itself. Encoded bytes flow writer, in-process queue, worker, destination.
- Reuse the existing `SegmentProcessor` pipeline. No new API for users who already wire `.s3(...)` or custom processors.
- Bounded memory with the same "drop oldest, never block the recorder" policy the disk path already uses for eviction. Memory mode evicts at ring overflow (drop-at-seal via `force_push`). Disk mode evicts after seal (`evict_oldest` in the writer).

## Non-goals

- Recovering in-flight segments after a process crash. Anything in memory at crash time is gone (the disk path already drops segments under eviction pressure).
- Spill-to-disk fallback when the memory budget is exhausted. A follow-up could make this configurable, but avoiding disk altogether keeps the feature usable in environments where disk access is unavailable.
- Single-file mode analogue (`RotatingWriter::single_file`) under `MemFs`. Memory mode always rotates.

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
          RotatingWriter
            │
            ├── Arc<dyn Fs> ── full segment lifecycle
            │   (DiskFs: real files | MemFs: per-path Vec<u8> + channel)
            │
            └── on seal:
                ├── disk: file is now on the filesystem ──▶ worker calls
                │       fs.take_files() (claim + lazy load)
                │
                └── memory: push (index, Bytes) into the ──▶ worker calls
                        internal channel + Notify              fs.take_files()
                                                                   │
                                                                   ▼
                                                              Pipeline (SegmentProcessor chain):
                                                                SymbolizeProcessor
                                                                GzipCompressor
                                                                S3PipelineUploader / user processor
```

The hot path (recording, thread-local flush, central collector) does not change. The work is on three pieces:

- A new `Fs` trait with `DiskFs` and `MemFs` impls
- The in-pipeline carrier (`SegmentRef` enum so processors operate in both modes).
- Backpressure observability symmetric across both backends (`WorkerCycleMetrics`).

### The `Fs` trait

Object-safe so `Arc<dyn Fs>` works.

```rust
pub trait Fs: Send + Sync + 'static {
    // --- Write side ---
    fn create(&self, path: &Path) -> io::Result<Box<dyn Write + Send>>;
    /// Seal the active segment. Disk: rename to the path produced by
    /// stripping the `.active` suffix. Memory: drain the active buffer
    /// and push to the internal channel.
    fn seal(&self, active: &Path, index: u32) -> io::Result<SegmentRef>;
    /// Eviction-side removal. Disk: unlinks the sealed file plus any
    /// extension-renamed siblings (e.g. `.gz` from `WriteBackProcessor`)
    /// and drops the claim entry. Memory: no-op (sealed bytes already
    /// left `MemFs` via `seal`).
    fn remove_sealed(&self, seg: &SegmentRef) -> io::Result<()>;
    fn remove_active(&self, path: &Path) -> io::Result<()>;

    // --- Read side ---
    /// Return claim receipts for newly visible sealed segments plus
    /// backpressure gauges sampled from the same scan. Each segment is
    /// handed out at most once per backend instance (see claim-set
    /// dedup below).
    fn take_files(&self) -> TakenFiles;
    /// Wait for new segments to potentially be available. Disk: sleeps
    /// `poll_interval`. Memory: awaits the channel `Notify`.
    fn wait_for_more<'a>(&'a self, stop: &'a CancellationToken, poll_interval: Duration) -> Pin<Box<dyn Future<Output = ()> + Send + 'a>>;
    /// Writer signaled it will produce no more segments. Memory: channel flag. Disk: always false (stop token handles shutdown).
    fn writer_done(&self) -> bool { false }
    /// Writer finalize hook. Memory: sets writer_done + notify.
    fn mark_writer_done(&self) {}
    /// `RotatingWriter::evict_oldest` hook. Disk: bumps the
    /// dropped-segments counter surfaced via `TakenFiles`.
    fn record_writer_eviction(&self) {}
}
```

`TakenFiles` carries the dispensed claims plus per-cycle gauges (`queued_count`, `queued_bytes`, `in_flight_count`, `in_flight_bytes`, `dropped_segments`). One scan per worker cycle drives both work and observability (see section 6 for metrics).

- **`DiskFs`** holds the segment directory and stem at construction and maintains a `HashMap<u32, u64>` of claimed indices to uncompressed sizes (under a `Mutex`). `take_files` scans the directory, stats each unclaimed file **outside the claim-set mutex**, then re-acquires the mutex once to insert all new entries. Keeping `metadata()` syscalls off the locked path matters because the writer's `evict_oldest` takes the same mutex (via `remove_sealed`). The `std::fs::read` for the payload itself is deferred to `TakenSegment::load`.
- **`MemFs`** holds active-only state (per-path `Vec<u8>` map) plus the internal writer-to-worker channel. Sealed bytes do not live in `MemFs`'s active map. `seal` takes the `Vec<u8>`, wraps it as `Bytes` (zero-copy) and pushes through the channel, which triggers eviction (see section 3). `take_files` pops **at most one** segment from the channel per call and returns it with payload already in hand. `remove_sealed` is a no-op: bytes already left `MemFs`.

### At-most-once handoff and lazy load

Without dedup, a stalled downstream causes the disk worker to re-read every accumulated `.bin` file every scan. `take_files` dispenses each file at most once per `Fs` instance. With the read deferred to `TakenSegment::load`, peak in-flight memory stays at one segment regardless of backlog.

`load()` returns `Err(NotFound)` when the file vanished between scan and load (last-stage cleanup raced the worker). The worker logs a rate-limited warn and skips. The next scan prunes the claim entry.

Memory mode dedups by construction, each ring slot pops exactly once.

### Worker loop

The worker holds an `Arc<dyn Fs>` and runs one loop:

```rust
loop {
    let r = self.fs.take_files();
    self.emit_cycle_metrics(&r);
    self.process_segments(r.segments).await;
    if stop || self.fs.writer_done() {
        let r = self.fs.take_files();      // final drain
        self.process_segments(r.segments).await;
        return;
    }
    if r.segments.is_empty() {
        self.fs.wait_for_more(&stop, self.poll_interval).await;
    }
}
```

### `RotatingWriter`

Holds `Arc<dyn Fs>`. Disk constructors wire `DiskFs::from_base_path(&trace_path)`. The memory constructor wires `MemFs::with_capacity(...)`, which owns the internal writer-to-worker channel. The worker reaches the same `Arc<dyn Fs>` via `TraceWriter::fs_handle()`. Constructor split and typestate covered in section 4.

Eviction lives where it is natural:

- **Disk:** writer holds `closed_files: VecDeque<(SegmentRef, u64)>`. After every rotation, `evict_oldest` pops the front and calls `Fs::remove_sealed` until the byte budget is satisfied. Identical to today.
- **Memory:** the channel enforces the byte budget. Push adds the segment, then drops oldest while `queued_bytes` is over `max_total_size`. The ring also has a slot cap (about `max_total_size / 4 KB` plus a bit of headroom) as a safety net for unusually small segments. `closed_files` stays empty.

### Rejected alternatives

- **`MemoryWriter` as a sibling `TraceWriter` impl.** Duplicates encoder/rotation/metadata/drain-timer logic. Tests still need `TempDir`. Single `RotatingWriter` over `Arc<dyn Fs>` is simpler.
- **Split write-side and read-side traits (`Fs` + `SegmentSource`).** Disk read-side is stateful (claim-set dedup) so the state would have to be shared across both traits anyway. Single trait keeps the lifecycle in one place.
- **Eager payload load in `take_files`.** Reads every unclaimed file into RAM on each scan. First drain after boot or recovery scales with backlog size. Lazy `TakenSegment::load` bounds peak in-flight memory to one segment.
- **`Fs::Writer` as an associated type.** `dyn Fs` not object-safe with associated types and `Arc<dyn Fs>` is needed at the recorder/worker boundary. Cost of `Box<dyn Write + Send>`: one `Box::new(File)` per rotation, one vtable call per ~8 KB BufWriter drain. Dominated by the syscall.
- **Sync `mpsc<()>(1)` for wakeup.** Already shuttle-shimmed, but blocking `recv()` would stall the current-thread worker, `spawn_blocking` per wait churns the thread pool.
- **`Mutex<VecDeque<MemSealedSegment>>` for queue.** Adds a sync mutex on a path other crate queues keep lock-free.
- **`crossbeam_queue::SegQueue`.** Unbounded, no eviction primitive.
- **`tokio::sync::mpsc::Receiver` carrying segments.** Built-in wakeup but no eviction, needs a side channel for overflow.
- **Writer-to-worker handoff as a continuous byte stream.** Breaks per-segment metrics, S3 multipart upload, retry semantics, the rotation contract, forces every processor to handle partial bytes.
- **Unified evict-after-seal for both modes.** Loses memory mode's natural ring byte bound and forces a sealed-state mutex.

---

## 2. Pipeline integration

Memory mode reuses the existing pipeline.

The only obstacle is `SegmentData::segment`, today typed as `SealedSegment { path, index }` and filesystem-coupled. Memory mode has no real path. `SealedSegment` stays as the disk variant, paired with a new `MemorySegment`, both under a `SegmentRef` enum:

```rust
pub struct SealedSegment { pub path: PathBuf, pub index: u32 }
pub struct MemorySegment { pub index: u32, pub size: u64 }

pub enum SegmentRef {
    Disk(SealedSegment),
    Memory(MemorySegment),
}

impl SegmentRef {
    pub fn index(&self) -> u32 { ... }
    /// Human-readable id for tracing and metric labels. 
    /// disk renders the path, memory renders `mem://{index}`.
    pub fn display_id(&self) -> impl std::fmt::Display + '_ { ... }
}
```

`SegmentData::segment()` returns `&SegmentRef`. Disk-only call sites (`WriteBackProcessor`, the path-logging branch of `S3PipelineUploader`) match on the enum. `WriteBackProcessor` paired with a memory writer is a compile error via the `PipelineBuilder` typestate (see section 4), so the disk-only `match` arm is defense in depth.

> **Breaking change.** `SegmentData::segment()` return type goes from `&SealedSegment` to `&SegmentRef`. External `SegmentProcessor` impls that called `data.segment().path()` need to match on the enum or read via `data.payload()`. Internal processors are updated in this change.

### Failure modes: retry budget, panic

Apply to every pipeline stage (memory or disk). Worker wraps each processor's `process(...)` future:

- **Bounded retry budget.** On `retryable: true`, the worker re-runs the stage up to `retry_budget` times before dropping the segment. Default 3. `S3PipelineUploader` wraps its own `CircuitBreaker` and is instantiated with `retry_budget = 1`. Memory mode needs this since segments leave the ring on pop. Without bounded retry, the first transient error would lose the segment.
- **Panic isolation.** Stage future runs inside `AssertUnwindSafe(...).catch_unwind()`. A panic drops the segment, fires metrics and keeps the stage instance (so transient state like connection pools survives). **Contract for custom processor authors:** a panicked `process(...)` must leave the instance valid for the next call (no held locks, no half-filled buffers, no corrupted state).

These attach to processors via `PipelineBuilder::pipe_with_config`.

---

## 3. Memory story

### What gets allocated

Three byte pools contribute to peak working set:

| Bucket | Owner | Size | Notes |
|--------|-------|------|-------|
| Active segment buffer | flush thread | up to `max_segment_size` | `Vec<u8>` inside the encoder. `RawEncoder` is a thin wrapper |
| Queued sealed segments | `BoundedQueue` ring | up to `max_total_size` | `MemSealedSegment { index, bytes: Bytes }` per slot. `Bytes` is a zero-copy Arc clone. Byte-bounded via `ChannelSender::push` eviction loop |
| In-flight pipeline data | worker | up to one `max_segment_size` + transient stage growth | Pipeline runs serial: at most one segment in-flight. Disk path bounds via lazy `TakenSegment::load`, memory via pop-one in `MemFs::take_files` (section 1). `SymbolizeProcessor` (cpu-profiling only) appends a symbol-table chunk. `GzipCompressor` output is a fraction of input. Benches will surface actual ratios. |

Note on interning: per-batch string interning lives on the recording thread, not the writer. Each `ThreadLocalBuffer` resets its encoder on every flush (`Encoder::reset_to_infallible`), so the interner peak is bounded by one batch encode, not a segment's life.

### Peak working set

`max_total_size` bounds the queue only. In-flight bytes are separate (the pipeline is serial, so at most one segment plus stage-internal growth is held outside the queue at any moment).

**Peak memory contract:** `max_total_size (queue) + max_segment_size (in-flight) + max_segment_size (active buffer)`.

Size `max_total_size` so the worker can absorb a 5-10x burst of slowness before drops fire. With 1 MB segments at 60s rotation that means 10 to 15 MB. Steady state is ~3 MB (one active, one in-flight, a small queue).

cpu-profiling adds symbol-table size to in-flight. Conservative rule-of-thumb: budget 2x `max_segment_size` for in-flight when cpu-profiling is on.

### Comparison to the disk path

Disk already pays for the active buffer and one in-flight segment in RAM (lazy `TakenSegment::load` keeps it at one regardless of backlog). Memory mode adds the queue itself, up to `max_total_size`, in process heap instead of as `.bin` files on disk. Encoded bytes are identical. Eviction fires under the same "worker can't keep up" trigger: disk via `evict_oldest`, memory via channel byte-budget.

### Byte accounting and drop-oldest

Memory-mode gauges on `ChannelReceiver`:

- **`queued_bytes`**: encoded bytes currently in the ring. The eviction-relevant gauge.
- **`in_flight_bytes`**: encoded bytes currently inside a `SegmentData` between worker pop and pipeline-end drop.
- **`in_flight_count`**: matching segment-count gauge. Memory-mode parity with disk-mode (where count is claim-map size, bytes is sum of claim-map values).
- **`dropped_segments`**: cumulative segments shed by channel byte-budget + slot-cap eviction.

The writer is the sole eviction source. The worker only consumes. Concurrent `queue.pop()` between writer eviction and worker consumption is safe because `BoundedQueue` is MPMC-safe, so each slot pops exactly once.

**Decrement-on-drop spec.** `SegmentData` carries an `accounting: Option<SegmentAccounting>` populated only for memory-backed segments:

```rust
struct SegmentAccounting {
    in_flight_bytes: Arc<AtomicU64>,
    in_flight_count: Arc<AtomicU64>,
    size: u64,
}

impl Drop for SegmentAccounting {
    fn drop(&mut self) {
        self.in_flight_bytes.fetch_sub(self.size, Ordering::AcqRel);
        self.in_flight_count.fetch_sub(1, Ordering::AcqRel);
    }
}
```

Disk-backed segments hold `accounting: None`. Drop fires regardless of pipeline outcome: success, retry-budget exhaustion, non-retryable failure, and panic all converge on `SegmentData::drop`. Panic case: the panicked future owns `SegmentData` and drops it inside `catch_unwind`, so `SegmentAccounting::drop` runs. The synthetic `SegmentData` the worker emits for panic metrics carries `accounting: None`. No double-decrement, no accounting handoff through the panic path. In-flight segments are out of the ring once popped, so eviction can't double-count them.

Stage-internal allocations (`SymbolizeProcessor` appending a symbol table, `GzipCompressor` flattening and compressing) don't update either gauge. Transient pipeline growth dies with `SegmentData` drop.

Drop signals: `dropped_segments` counter increments per evicted segment (see section 6 metrics catalog), plus a rate-limited `tracing::warn!` so sustained overruns are visible in logs.

---

## 4. API surface

Memory mode is one constructor swap. The writer's `Arc<dyn Fs>` is the only handle the runtime builder needs.

```rust
let writer = RotatingWriter::in_memory(
    /* max_segment_size */ 1 * MB,
    /* max_total_size   */ 16 * MB,
)?;
let (runtime, guard) = TracedRuntime::builder()
    .with_s3_uploader(s3_config)
    .build_and_start(tokio_builder, writer)?;
```

### Mode handling

`RotatingWriter` carries a phantom `Mode` typestate: `RotatingWriter<Disk>` (default) or `RotatingWriter<Memory>`.

```rust
pub struct RotatingWriter<Mode = Disk> { /* ... */ }
pub trait WriterMode: sealed::Sealed {}
pub struct Disk;
pub struct Memory;
impl WriterMode for Disk {}
impl WriterMode for Memory {}
```

Constructors split:

- `new`, `single_file`, `builder` return `RotatingWriter<Disk>`.
- `in_memory` returns `RotatingWriter<Memory>`.

Builder constraint:

- `HasTracePath::build_and_start(rt, w: RotatingWriter<Disk>)`. Memory writer here is a compile error.
- `NoTracePath::build_and_start(rt, w: RotatingWriter<Memory>)`. Memory-mode entry point.
- `NoTracePath::build_and_start_with_writer(rt, w: impl TraceWriter)` keeps the generic path for `NullWriter`, custom writers and test doubles. Small breaking change: `NullWriter` / test-double callers that previously invoked `build_and_start` migrate to `build_and_start_with_writer`.

`RotatingWriter<Mode>` is `PhantomData<Mode>` at runtime. Storage is mode-agnostic. A `tracks_closed_files: bool` field controls whether the writer runs `evict_oldest` after rotation (disk only). `finalize` unconditionally calls `Fs::mark_writer_done`. `DiskFs`'s default impl is a no-op so disk mode pays nothing.

#### Preventing misuse

Pipeline mode is also enforced at compile time via `PipelineBuilder<DiskReq = NoDiskRequired>`. `.write_back()` transitions to `PipelineBuilder<RequiresDisk>`. Memory-writer entry points constrain `DiskReq = NoDiskRequired`, so attaching `WriteBackProcessor` to a memory writer is a compile error. User-supplied processors that need disk backing opt in via a `DiskBoundProcessor` marker trait and use `pipe_disk_bound(p)` instead of `pipe(p)`.

### `RotatingWriter::in_memory` defaults

- `rotation_period`: reuses disk's `DEFAULT_ROTATION_PERIOD` (60 s). Caps how long an event can sit in the active buffer when traffic is low.
- `drain_interval`: internal.
- `queue_capacity` (slot-floor): derived from `max_total_size / 4 KB` plus headroom. The user-facing budget is bytes. The slot count is a safety net for unusually small segments.

`in_memory` returns `Err(InvalidInput)` if `max_segment_size > max_total_size`. Disk silently halts in this case after one rotation (`evict_oldest` flips `WriterState::Finished`) but the explicit memory constructor has clearer ergonomics.

### Rejected alternatives

- **`take_pending_receiver` on `TraceWriter` plus writer-held `pending_receiver: Option<ChannelReceiver>`.** Trait method overridden only on `RotatingWriter` plus a writer field drained once at build time. `MemFs` owns the channel and the worker pulls it via `TraceWriter::fs_handle()`.
- **`MemoryPipeline { writer, receiver }` newtype.** Same problem: an extra public type users carry around for a transient setup step.
- **`builder.with_memory_writer(seg, total)` high-level method.** Loses writer-level config knobs (`rotation_period` etc.) at the builder level or forces per-knob builder methods.

---

## 5. Crash and shutdown semantics

Graceful shutdown for memory mode mirrors disk:

1. Stop the flush thread.
2. `RotatingWriter::finalize`: seal the active segment via `Fs::seal` (`MemFs` pushes onto the ring) and signal `writer_done`. The worker observes `writer_done` on its next wait and exits after one final drain.
3. `TelemetryGuard::graceful_shutdown(timeout)` waits for the worker to exit.

Disk follows the same steps without `writer_done`. The worker's stop-token plus drain-once-more handles shutdown.

**Ordering invariant:** Three sync points, the final segment is always delivered before the worker exits:

1. **Writer seal:** `ChannelSender::push` does `queued_bytes.fetch_add(size, AcqRel)`, then `BoundedQueue::force_push`, which establishes a Release synchronization point on the queued slot. The byte-eviction loop that follows may pop additional slots, but those don't gate the just-pushed slot.
2. **Writer mark-done:** after the final `seal`, `RotatingWriter::finalize` calls `Fs::mark_writer_done`. On `MemFs` this does `writer_done.store(true, Release)` then `notify_one`.
3. **Worker observe + re-drain:** `Fs::wait_for_more` on `MemFs` does `writer_done.load(Acquire)`. Once it observes `true` the worker exits the wait, the run loop falls through to one final `take_files` and `BoundedQueue::pop` performs the `Acquire` on the slot. This is guaranteed to see step 1's push because the write to `writer_done` (Release) synchronizes-with the load (Acquire) and the queue push happens-before the `mark_done` store on the writer thread.

Lost-wakeup avoidance is handled by the standard `notified() / enable() / re-check writer_done / await` pattern inside `MemFs::wait_for_more`. The future is registered before the second `writer_done` load so any `notify_one` that fires between the first load and the await becomes the permit the await consumes.

---

## 6. Testing

Reuse the patterns from the disk path. The headline change is that **`RotatingWriter` tests can swap `MemFs` in for the `Arc<dyn Fs>` backing store**, dropping `TempDir` for existing cases.

**Unit coverage.** `Fs` impls (active map transitions, seal pushes and drains correctly, `Bytes` zero-copy), channel ordering and accounting, claim-set dedup on `DiskFs` (each file dispensed at most once), lazy payload (`TakenSegment::load` defers reads, NotFound between scan and load yields `Err`), writer rotation/eviction/metadata against `MemFs` (small disk-only subset stays on `DiskFs` for rename atomicity and dir-scan edge cases).

**Integration.** Memory `RotatingWriter` paired with a test `SegmentProcessor` that captures `SegmentData`. Run a workload, confirm bytes decode and event counts match. S3 integration against `s3s` (already used by the disk-path S3 tests).

**Stress + Shuttle.** High segment-emission rate against a slow downstream processor: verify drop-oldest fires, counters increment, writer never blocks. Shuttle scenarios: concurrent `force_push` / `pop`, shutdown race on `writer_done`, eviction-under-contention accounting, `SegmentData` drop racing with `force_push` eviction (no underflow).

### Memory regression tests

Memory mode keeps bytes in process heap that disk kept on disk. Regressions are easier to introduce and harder to spot. Two layers to be checked on PRs:

- **Counter assertions.** Fixed-seed workload against a memory `RotatingWriter`. Assert peak `queued_bytes` stays within `max_total_size`, `in_flight_bytes` stays within `max_segment_size`, eviction fires when the budget is intentionally tight, queue depth stays bounded. Catches accounting regressions.
- **Heap baseline.** Same workload under a deterministic heap profiler (e.g. `dhat`), baseline checked into CI. Catches leaks and allocator-side regressions.

### Metrics

`FlushMetrics`, `SegmentProcessMetrics` and `TlDrainMetrics` cover writer, flush and per-segment paths as today. New: a per-cycle `WorkerCycleMetrics` entry (`Operation::WorkerCycle`) emitted once per `take_files` call, symmetric across disk and memory:

- `QueuedCount` / `QueuedBytes`: segments visible to the backend but not returned this cycle. Reserved for bounded-take semantics, 0 in steady state today.
- `InFlightCount` / `InFlightBytes`: segments claimed but not yet released by last-stage cleanup or `remove_sealed`. Rising values mean the pipeline is not shedding work fast enough.
- `DroppedSegments`: backend-side evictions. Disk: `RotatingWriter::evict_oldest` (via `Fs::record_writer_eviction`). Memory: channel byte-budget plus slot-cap.
- `SegmentsDispatched`: segments handed into the pipeline this cycle.

Fires every cycle, drained-empty included, so a stuck pipeline shows climbing `InFlightBytes` with `SegmentsDispatched == 0`. `ChannelReceiver` keeps direct accessors for at-cadence sampling.

---

## 7. Open questions

**Adaptive sizing:** The current design takes a static `max_total_size`. We could measure queue depth and let the writer grow/shrink within a configured range, similar to how some allocators auto-tune their arenas. Perhaps container users could benefit more from reading cgroup `memory.max` / `memory.high` as an outer cap. Initially keeping it out of scope for v1 or at least until implementation testing shows workload data. The metrics catalog (`QueuedBytes`, `QueueDepth`, `DroppedSegments`) covers the internal inputs we'd need.
