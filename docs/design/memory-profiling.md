# Memory Profiling

## Overview

Add sampled allocation profiling to dial9. A `Dial9Allocator<A>` wraps any
`GlobalAlloc` (default `System`) and emits `AllocEvent` / `FreeEvent` events
into the trace on sampled allocations. Stacks are captured via the frame
pointer unwinder already used for CPU profiling. The viewer and analysis
toolkit gain allocation flamegraphs, per-task allocation totals, and leak
detection.

The hot path on the allocating thread is **bare bones**: sample decision,
stack capture, push a fixed-size POD record into a process-global lock-free
queue. A dedicated **consolidator thread** drains the queue and turns each
record into the corresponding trace event. This keeps the allocator hook
allocator-quiet by construction and lets the consolidator use ordinary
`HashMap`/encoder machinery without re-entering the hook.

The design mirrors jemalloc's and Go's allocation profilers: **geometric
(Poisson) sampling** keyed on allocation size. The expected sampling rate
is 1 sample per N bytes allocated, regardless of object size distribution.
This gives unbiased size-weighted profiles at bounded overhead.

**Why not delegate to jemalloc's built-in profiling?** Our wrapper works
with *any* `GlobalAlloc` (system, jemalloc, mimalloc), integrates directly
into the dial9 trace (same timeline, same viewer, same task attribution),
and reuses the existing frame-pointer unwinder. Tradeoff: no access to
allocator internals (bin sizes, arena stats) — for that, use jemalloc's
`prof` directly.

## Goals

- Always-on in production, with sub-1% overhead at default sampling rates.
- Works with any `GlobalAlloc` (system, jemalloc, mimalloc, etc.) via a
  zero-cost wrapper.
- Stacks tied to the worker thread + task that performed the allocation, so
  allocation hot paths can be attributed to specific tasks or poll ranges.
- Optional live-set tracking for leak detection. Off by default — it
  adds an extra ring push per dealloc and a per-sample lookup on the
  consolidator side.
- No symbolication on the hot path. Addresses flow through the existing
  background `SymbolizeProcessor`.
- **Install-once, captured handle.** `MemoryProfiler::install(handle)`
  sets a process-global static that the allocator reads. The captured
  handle routes all sampled events through the central recorder.
- **Instruments all threads**, not just tokio workers — allocations on
  tokio workers, blocking pool, user OS threads, and library-spawned
  threads are all recorded identically.
- **Viewer and analysis toolkit support.** Allocation flamegraphs,
  per-task allocation totals, and leak detection views ship alongside
  the backend instrumentation.

## Non-goals

- Tracking every allocation. We sample.
- Replacing heaptrack / valgrind. Those record every allocation and are
  fine for dev-time analysis. Dial9 is for production, always-on.
- Detecting use-after-free, double-free, or other memory safety bugs.
  That's what Miri and ASAN are for.
- **Inspecting memory contents.** We record size, address, and stack —
  never the bytes stored in the allocation. No PII/secret leakage risk.
- **Cryptographic randomness.** The per-thread RNG is a fast PRNG
  (SplitMix64) seeded from a user-supplied or random seed. It is not
  cryptographically secure and doesn't need to be.
- Tracking stack allocations or `mmap`-backed memory outside the
  allocator. **Evolution path:** once the core allocator profiling is
  stable, we can add `mmap`/`munmap` interception to cover large
  anonymous mappings that bypass the allocator.

---

## 1. Geometric sampling

Per-thread byte counter `next_sample_bytes`. On every allocation of size
`s`:

```rust
fn on_alloc(size: usize) {
    // i64: must be signed — subtracting a large `size` from a small
    // remaining counter must go negative, not wrap around.
    let remaining = next_sample_bytes.get() - size as i64;
    if remaining > 0 {
        next_sample_bytes.set(remaining);
        return;               // fast path: one sub, one branch, done
    }

    // Sampled. Draw a new gap — loop in case the draw is smaller than
    // `-remaining` (rare, but possible for tiny rates / huge allocs).
    let mut next = remaining;
    while next <= 0 {
        next += draw_exponential(sample_rate_bytes);
    }
    next_sample_bytes.set(next);

    record_sample(size, capture_stack());
}
```

Two important details:

- The `while` loop handles the case where a single huge allocation (or
  very low sample rate) pushes `next_sample_bytes` below zero by more
  than one draw's worth.
- `remaining` must be `i64` (signed). Subtracting `usize` from `usize`
  and comparing to zero invites wraparound bugs.

Default `sample_rate_bytes = 512 KiB`. At that rate, a service doing 1 GB/s
of allocation generates ~2000 samples/sec — plenty of signal, trivial
overhead.

### RNG

**Per-thread RNG in TLS, not process-global.** A shared `AtomicU64` would
force a read-modify-write across threads on every allocation; TLS gives
each thread its own state with a plain load/store.

**Reuse the existing `SplitMix64` and `draw_exponential` from
`task_dumped.rs`**, extracted into a shared `pub(crate)` module
(e.g. `dial9-tokio-telemetry/src/sampling.rs`). The shape is identical —
only the unit changes (`bytes` instead of `nanoseconds`). Shared API:

```rust
// dial9-tokio-telemetry/src/sampling.rs
pub(crate) struct SplitMix64(u64);

impl SplitMix64 {
    pub(crate) fn new(seed: u64) -> Self;
    pub(crate) fn next_u64(&mut self) -> u64;
    /// Draw from exponential distribution with the given mean.
    /// Always returns at least 1 to avoid immediate re-trigger.
    pub(crate) fn draw_exponential(&mut self, mean: u64) -> i64;
}
```

Memory profiling stores per-thread state in TLS:

```rust
thread_local! {
    static SAMPLE_STATE: Cell<SamplingState> = Cell::new(
        SamplingState::new(global_seed().wrapping_add(thread_nonce()))
    );
}

struct SamplingState {
    next_sample_bytes: i64,
    rng: SplitMix64,
}
```

Each thread seeds its state lazily on first sample from a shared
install-time seed mixed with a per-thread nonce (e.g.
`ThreadId::as_u64()` or a counter), so the stream is deterministic
given `rng_seed` and reproducible across runs for tests.

Short-lived threads that never sample pay zero: `thread_local!`
initializers don't run until the first access.

### Why geometric over alternatives

Unbiased estimates of total bytes allocated per call site. Reservoir
sampling biases against allocations that occur late in a program's
lifetime. Fixed N-of-every-M biases against large objects that get
undersampled. Per-byte Bernoulli sampling via the geometric/exponential
trick gives the lowest variance estimator of the simple strategies.

### Small-object unbiasing

When you sample at rate `R` and see an allocation of size `s < R`, the
*expected* size represented is `R / (1 - exp(-s/R))`. The analysis toolkit
applies this to produce unbiased byte totals. Jemalloc uses the same
formula. Aggregation must happen *after* unbiasing per sample —
sum-then-unbias underreports small-object stacks.

---

## 2. Stack capture

The memory profiler uses the existing `Unwinder` (from
`dial9-perf-self-profile`) for stack capture. Key properties:

- **No allocations.** Captures into an on-stack `[u64; 128]` buffer.
- **Safe against corrupted frame chains** via the `safe_load` SIGSEGV
  handler (installed once at startup).
- **~5 ns per frame, ~110 ns for a 20-frame walk** on x86_64. Add
  ~50–200 ns in production for cold caches; faulting frames that hit
  the SIGSEGV safe-load path cost ~1–5 µs for the single faulting
  frame.
- **Requires frame pointers** (`-C force-frame-pointers=yes`).

`MemoryProfiler::install()` calls `Unwinder::install()` and stores the
returned handle. The hook accesses it via the process-global
`MemoryProfilerInner`:

```rust
// 128 frames × 8 B = 1 KiB stack buffer. Rust async call stacks
// routinely exceed 40 frames, so 128 gives comfortable headroom.
let mut frames = [0u64; 128];
let n = inner.unwinder.capture(&mut frames);
```

**Known limitation:** Applications that install their own SIGSEGV handler
after dial9 initialization may break the unwinder's fault tolerance.

---

## 3. Events

### `AllocEvent`

```rust
#[derive(TraceEvent)]
struct AllocEvent {
    #[traceevent(timestamp)]
    timestamp_ns: u64,
    /// OS thread ID. Matches CpuSample.tid.
    tid: u32,
    /// Allocation size in bytes.
    size: u64,
    /// Returned pointer. Only meaningful when liveset tracking is on —
    /// otherwise 0. Field always present so the schema is stable.
    addr: u64,
    /// Stack at the allocation site.
    stack: InternedStackFrames,
}
```

**Sampling rate in metadata, not per-event.** The sample rate is
immutable for the life of a trace file and is written into
`SegmentMetadata`:

```
SegmentMetadata["memory_profile.sample_rate_bytes"] = "524288"
```

The analysis toolkit reads `sample_rate_bytes` from segment metadata
and applies it uniformly to every `AllocEvent` when computing unbiased
byte totals. If we later want a `set_sample_rate_bytes` API, it will
atomically update the rate and force a segment rotation so the new
segment's metadata carries the new rate.

**Why explicit `tid`?** A trace can contain allocations from threads
that aren't inside a poll — blocking-pool workers, user-spawned OS
threads, early-boot allocations. `CpuSample` already carries `tid` for
the same reason; alloc events and CPU samples join cleanly on thread.

We **don't** carry a `worker_id`: when the alloc happens on a tokio
worker, the worker's identity is recoverable by joining `tid` to the
most recent `WorkerUnparkEvent.tid` ≤ `AllocEvent.timestamp_ns`. This
keeps the allocator hook decoupled from tokio runtime state.

> **Pre-requisite**: `WorkerParkEvent` / `WorkerUnparkEvent` must
> include `tid`. That's a small additive change to those event schemas.

### `FreeEvent` (liveset only)

```rust
#[derive(TraceEvent)]
struct FreeEvent {
    #[traceevent(timestamp)]
    timestamp_ns: u64,
    /// OS thread ID of the thread that ran the dealloc.
    tid: u32,
    /// Pointer that was freed. Matches a previously-seen `AllocEvent.addr`.
    addr: u64,
    /// Size of the allocation being freed.
    size: u64,
    /// Monotonic-ns timestamp of the original `AllocEvent`.
    alloc_timestamp_ns: u64,
}
```

**Why denormalize `size` and `alloc_timestamp_ns`?** `RotatingWriter`
evicts old segments. A long-lived allocation will have its `AllocEvent`
in a segment that has been evicted by the time its `FreeEvent` is
written. Denormalizing keeps `FreeEvent` self-sufficient for
net-bytes-freed and generational leak analysis across rotation.

We do **not** denormalize the allocation stack onto `FreeEvent`:
storing the full stack in every liveset entry would bloat the liveset
~8× and that memory is paid while the allocation is live.

### `realloc` handling

**Follow jemalloc's approach.** Treat `realloc(p, n_bytes)` as:
1. `dealloc(p, old_layout)` — may emit `FreeEvent` if `p` was sampled.
2. `alloc(new_layout)` — fresh sampling decision, may emit `AllocEvent`.

In-place realloc (pointer unchanged) still goes through the same flow.
Timestamps disambiguate the free-then-alloc of the same address.

### Worker / task attribution

Both events carry an implicit task_id via the shared `PollStart` context.
Every alloc inside a poll falls between that worker's most recent
`PollStart` and the matching `PollEnd`. The analysis toolkit already uses
this range-matching for CPU samples.

Allocations outside any poll carry only `tid` and get no task
attribution. The viewer shows these in a "blocking" or "unknown" lane.

---

## 4. `Dial9Allocator<A>`

Generic wrapper, default `A = System`:

```rust
pub struct Dial9Allocator<A = std::alloc::System>(A);

impl Dial9Allocator {
    pub const fn system() -> Self { Self(std::alloc::System) }
}

impl<A: GlobalAlloc> Dial9Allocator<A> {
    pub const fn new(inner: A) -> Self { Self(inner) }
}

unsafe impl<A: GlobalAlloc> GlobalAlloc for Dial9Allocator<A> {
    unsafe fn alloc(&self, layout: Layout) -> *mut u8 {
        let ptr = unsafe { self.0.alloc(layout) };
        if !ptr.is_null() {
            hook::on_alloc(ptr, layout.size());
        }
        ptr
    }

    unsafe fn dealloc(&self, ptr: *mut u8, layout: Layout) {
        hook::on_dealloc(ptr, layout.size());
        unsafe { self.0.dealloc(ptr, layout) };
    }

    unsafe fn realloc(
        &self, ptr: *mut u8, old_layout: Layout, new_size: usize
    ) -> *mut u8 {
        let new_ptr = unsafe { self.0.realloc(ptr, old_layout, new_size) };
        if !new_ptr.is_null() {
            // Only record the free-of-old after confirming realloc
            // succeeded. If realloc returns null, the old pointer is
            // still live and must not be recorded as freed.
            hook::on_dealloc(ptr, old_layout.size());
            hook::on_alloc(new_ptr, new_size);
        }
        new_ptr
    }
}
```

User code:

```rust
// Common case: system allocator.
#[global_allocator]
static ALLOC: Dial9Allocator = Dial9Allocator::system();

// Wrapping a custom allocator:
#[global_allocator]
static ALLOC: Dial9Allocator<tikv_jemallocator::Jemalloc> =
    Dial9Allocator::new(tikv_jemallocator::Jemalloc);
```

The wrapper is zero-cost when memory profiling isn't installed (see §6).

---

## 5. Ring-buffer architecture

The allocator hook does the **bare minimum** on the allocating thread —
sampling decision, stack capture, push a fixed-size POD record into a
lock-free queue. The existing **flush thread** (which already drains
`CpuProfiler` and `SchedProfiler`) picks up the memory profiler as
another `Source` and turns each record into the corresponding trace event.

**Why a ring buffer?** Everything an allocator hook touches must be
allocator-quiet. Encoders allocate. Concurrent hashmaps allocate.
`tracing::warn!` allocates. Each of those becomes a potential deadlock
or infinite recursion when run from inside `GlobalAlloc::alloc`. The
reentrancy concerns are trickier than they appear — TLS destruction
ordering, lock re-entry from encoder allocations, and epoch-reclamation
panics in concurrent data structures all create subtle hazards. A
ring-buffer hand-off eliminates the entire class by construction: the
hook never holds a lock, never calls into data structures with their own
TLS, and never grows a collection.

### Two queues

The design uses **two separate `ArrayQueue`s**:

1. **Alloc queue** — `ArrayQueue<RawAlloc>`, 4096 slots × ~520 B ≈ 2.1 MiB.
   Receives sampled allocations (large records with full stack frames).
2. **Free queue** — `ArrayQueue<RawFree>`, 32K slots × 24 B ≈ 768 KiB.
   Receives dealloc notifications (small records: addr + tid + timestamp).
   Only active when liveset tracking is on.

The size asymmetry reflects the traffic pattern: allocs are rare (sampled,
~2K/sec at default rate) but large (carry stack frames); frees are frequent
(every dealloc of a sampled pointer) but tiny.

### What the allocator hook does

1. Load process-global `OnceLock<MemoryProfilerState>`. Return early if unset.
2. Subtract `size` from the per-thread `next_sample_bytes` counter (~1 ns).
3. **Unsampled (~99.9% of allocs):** return.
4. **Sampled:**
   - `Unwinder::capture(&mut frames)` into an on-stack `[u64; 128]` (~110 ns).
   - Build a `RawAlloc { tid, size, ts_ns, addr, frames, frame_count }` on
     the stack.
   - `alloc_queue.push(sample)` — lock-free MPMC push (~10–30 ns uncontended).
   - On queue full: increment a dropped-samples counter, continue.

For deallocs (liveset on):
- Push `RawFree { tid, addr, ts_ns }` to the free queue (~9 ns uncontended).

No mutex. No encoder. No hashmap. No `Vec::with_capacity`.

### Consolidator (flush thread)

The flush thread drains both queues every 5 ms via the `Source` trait.
**Drain order: merge by timestamp** — peek both queues, pop whichever has
the older timestamp, repeat. This handles in-place realloc correctly
(free-of-old at T₁ must be processed before alloc-of-new at T₂ for the
same address).

For each `RawAlloc`:
1. Intern the stack via the flush thread's `ThreadLocalEncoder`.
2. Encode an `AllocEvent` into the trace.
3. If liveset is on, insert into the consolidator's `HashMap<usize, LivesetEntry>`.

For each `RawFree`:
1. Look up `addr` in the liveset `HashMap`. Hit → emit `FreeEvent`. Miss → ignore.

### Why `crossbeam_queue::ArrayQueue`

- Producers come from arbitrary threads. A per-thread SPSC ring would
  require per-thread setup that itself allocates.
- `ArrayQueue` is wait-free for `push` (CAS on a single tail slot index)
  and the workspace already depends on `crossbeam-queue`.
- At default 512 KiB sample rate and 1 GB/s allocation, the system pushes
  ~2K samples/sec — invisible contention even on 32 cores.

### Reentrancy

The flush thread's own allocations (interning stacks, growing the
encoder's hashmaps, inserting into the liveset) *do* trigger the hook.
This is fine and intentional:

- The hook on the flush thread does the same sampling decision as any
  other thread (~1 in 8000 allocs at default rate).
- Sampled allocations push a `RawAlloc` into the ring — the push is
  allocator-quiet, so it can't cascade.
- dial9's own allocation pressure shows up in the trace. If the profiler
  itself generates noticeable traffic, that's information we want. Filter
  `dial9_tokio_telemetry::*` frames at analysis time if needed.

**Geometric self-sampling is bounded.** The flush thread drains N samples,
encoding them does ~kN allocations, ~kN/8000 trip the sampler, producing
k²N/8000² second-order self-samples. The geometric series converges fast;
steady-state self-pollution is ~0.01% of trace events.

**Allocation during stack capture.** `Unwinder::capture()` never allocates —
it uses only the on-stack `[u64; 128]` buffer and the allocation-free
frame-pointer unwinder.

**Early-boot allocations.** Before `MemoryProfiler::install()`, the
`OnceLock` is unset and the hook returns early (~1 ns overhead).

---

## 6. Configuration — static install, captured handle

`MemoryProfiler::install()` sets a process-global static that the
`Dial9Allocator` reads on every allocation. Installation happens
**exactly once per process** and takes a `TelemetryHandle` captured
at install time.

### Shape

```rust
use dial9_tokio_telemetry::memory_profiling::{
    Dial9Allocator, MemoryProfiler, TimestampMode,
};

#[global_allocator]
static ALLOC: Dial9Allocator = Dial9Allocator::system();

fn main() {
    let guard = TelemetryCore::builder()
        .writer(writer)
        .trace_path("/tmp/trace.bin")
        .build()?;
    guard.enable();

    let _mem = MemoryProfiler::builder()
        .sample_rate_bytes(512 * 1024)
        .track_liveset(true)
        .timestamp_mode(TimestampMode::ReusePollStart)
        .install(guard.handle())?;

    let (rt, _) = guard.trace_runtime("main").build(rt_builder)?;
    rt.block_on(async { /* ... */ });
}
```

Between the global allocator static and `install()`, every allocation
takes the unset-`OnceLock` fast path: one `Acquire` load + null check
(~1 ns), not set → skip.

### Why capture the handle

There's one global allocator, allocations come from everywhere, and we
want *all* of them to land in the same trace. A single captured handle
achieves that — no `TelemetryHandle::current()` TLS lookup on the hot
path.

### `MemoryProfiler::install()` flow

```rust
static ACTIVE: OnceLock<MemoryProfilerState> = OnceLock::new();

impl MemoryProfiler {
    pub fn install(
        self,
        handle: TelemetryHandle,
    ) -> Result<MemoryProfilerGuard, InstallError> {
        let unwinder = Unwinder::install().map_err(InstallError::Unwinder)?;
        let alloc_ring = Arc::new(ArrayQueue::new(self.config.ring_capacity));
        let free_ring = Arc::new(ArrayQueue::new(self.config.ring_capacity * 8));

        let state = MemoryProfilerState {
            unwinder,
            config: self.config.clone(),
            alloc_ring: alloc_ring.clone(),
            free_ring: free_ring.clone(),
        };

        ACTIVE
            .set(state)
            .map_err(|_| InstallError::AlreadyInstalled)?;

        handle.register_source(MemoryProfileSource {
            alloc_ring,
            free_ring,
            liveset: self.config.track_liveset.then(HashMap::new),
        });

        Ok(MemoryProfilerGuard { _private: () })
    }
}
```

The state lives in `OnceLock` and is never reclaimed — in-flight hook
calls may be reading it at any moment on any thread. Cost is ~100 bytes
plus the ring allocations. The `OnceLock` encodes the "write once, read
many, never reclaim" invariant in the type system.

If `install()` is called with a disabled handle, the `OnceLock` is not
set — no wasted sampling work.

### Hot path

```rust
fn alloc(&self, layout: Layout) -> *mut u8 {
    let ptr = unsafe { self.0.alloc(layout) };
    if !ptr.is_null() {
        if let Some(state) = ACTIVE.get() {
            hook::on_alloc(state, ptr, layout.size());
        }
    }
    ptr
}
```

### `MemoryProfilingConfig`

```rust
#[derive(Debug, Clone)]
pub struct MemoryProfilingConfig {
    sample_rate_bytes: u64,             // default 512 KiB
    track_liveset: bool,                // default false
    timestamp_mode: TimestampMode,      // default ReusePollStart
    max_liveset_entries: Option<usize>, // default None (unbounded)
    rng_seed: Option<u64>,              // test-only deterministic seeding
    ring_capacity: usize,              // default 4096 slots
}

/// How `AllocEvent.timestamp_ns` is populated.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[non_exhaustive]
pub enum TimestampMode {
    /// Reuse the timestamp from the most recent `PollStart` on this
    /// thread when available (~2 ns TLS load). Falls back to
    /// `clock_monotonic_ns()` on threads with no recorded `PollStart`.
    /// Default.
    ReusePollStart,

    /// Emit events with `timestamp_ns = 0`. Smallest on-disk size.
    /// Analysis can still group by stack and size; loses time-range
    /// filtering.
    None,

    /// Call `clock_monotonic_ns()` per sampled allocation (~25 ns via
    /// vDSO). Use for tight allocation loop investigations.
    Precise,
}
```

Built via `#[bon::builder]` as with other dial9 configs.

### Graceful shutdown

On graceful shutdown, the flush thread runs one final drain cycle.
For applications that need the absolute last batch of events on disk:

```rust
guard.handle().flush_memory_profile()?;  // synchronous drain
guard.graceful_shutdown(Duration::from_secs(5))?;
```

---

## 7. Liveset tracking

When `track_liveset = true`, the **consolidator thread** maintains:

```rust
struct LivesetEntry {
    size: u64,
    timestamp_ns: u64,
}

// Single-threaded — only the consolidator reads or writes it.
liveset: HashMap<usize, LivesetEntry>,
```

The liveset is a plain `HashMap` because only the consolidator touches
it. Producers push `RawFree` records into the free queue and never
access the liveset directly — no synchronization needed.

### Bounded liveset

`max_liveset_entries` caps the total count. When full:
- New `RawAlloc`: still emit `AllocEvent`, skip liveset insert. Emit a
  rate-limited warning event (once per 60s).
- `RawFree`: still lookup (miss on overflow entries). No-op on miss.

Default: `None` (unbounded). Users opt in to a cap.

### Memory cost

~32 bytes per live sampled allocation (16 B entry + 16 B HashMap
overhead). At default 512 KiB sample rate and 1 GiB live heap: ~2K
entries → ~64 KiB. At 10 GiB live heap: ~20K entries → ~640 KiB.

### Dealloc overhead

Pushing `RawFree` records costs ~9.3 ns uncontended per dealloc. The
MPMC queue saturates at ~2.7 M pushes/sec aggregate under heavy
contention. For services with 10M+ deallocs/sec, a producer-side
optimization is needed:

- **Per-thread free buffer:** batch frees in a TLS `[RawFree; 64]`
  array; flush to the free queue every 64 frees. Converts 64 contended
  pushes into 1.
- **Producer-side bloom filter:** test whether the address was sampled
  (~5 ns hash + bit-check) before pushing; 99.9% of deallocs skip the
  push entirely.

Pick at implementation time based on whether production workloads show
actual contention. Consolidator-side `HashMap` miss for unsampled
addresses is ~10–20 ns — not worth optimizing until profiling shows
otherwise.

Liveset is off by default because even 9.3 ns per dealloc adds up on
dealloc-heavy workloads.

---

## 8. Overhead budget

Target: <1% at default settings (512 KiB sample rate, no liveset).

**Context:** A single `malloc`/`free` on modern allocators takes ~20-80 ns
uncontended. Our fast-path overhead (~1 ns) is well within the noise.

**Per-allocation fast path (unsampled, ~99.9% of calls):**
- 1 `OnceLock::get()` (Acquire load + null check, ~1 ns)
- 1 subtract + compare on per-thread `next_sample_bytes` (~1 ns)
- Total: **~1.1 ns**

For a service doing 1M allocs/sec: ~1 ms/sec of CPU per core (0.1%).

**Per-sampled allocation (~0.1% of calls):**
- Stack capture: ~110 ns (20 frames warm-cache; ~200–400 ns cold)
- Build `RawAlloc` on stack: ~30 ns
- `ArrayQueue::push`: ~50–100 ns uncontended
- Optional timestamp: ~25 ns (`TimestampMode::Precise`)
- Total: **~1226 ns** (dominated by stack capture)

At 2K samples/sec: ~2.5 ms/sec of CPU per core (0.25%).

**Dealloc (liveset on):** ~9.3 ns per dealloc uncontended.

### Integration benchmark

The implementation ships with a benchmark exercising the full
`Dial9Allocator → ring → consolidator → trace` pipeline:
- High-frequency small allocations (`Box::new(T)` loops)
- Realloc growth (`Vec::push` loops)
- Mixed sizes across the sample-rate boundary
- Comparison: (no profiler) vs (sampling only) vs (sampling + liveset)

---

## 9. Viewer changes

### Allocation flamegraph

Same UX as CPU flamegraph — click a poll, or shift-drag a time range,
see a flamegraph. Sample value is
`AllocEvent.size * weight_correction(AllocEvent.size, sample_rate)`,
where `sample_rate` is read from `SegmentMetadata`.

### Per-task allocation chart

For each task, total bytes allocated + sampled count. Sort by size;
shows leaky / hot tasks at a glance. Uses the same poll-range join the
viewer already does for `CpuSample`. Allocations with no containing
poll go into an "unassociated" bucket.

### Leak view (liveset only)

Show allocations with no matching free at end-of-trace, grouped by
stack, sorted by total bytes.

---

## 10. Analysis toolkit

Extend `dial9-viewer/skills/analyze.js` with:

- `allocationsByTask()` — groups by task, weighted.
- `topAllocationStacks(n)` — flamegraph-style stack aggregation, unbiased.
- `leakCandidates(minBytes)` — live allocations grouped by stack, above
  a threshold.

New skill doc: `dial9-viewer/skills/memory.md` covering recipes for
common questions ("what allocated the most bytes in this time range?",
"which stacks show the largest retained heap?", etc.).

---

## 11. Testing strategy

`MemoryProfiler::install` publishes a process-global `OnceLock` that is
never reclaimed. Tests that exercise different configurations each need
their own process. Use separate `tests/memory_profiling_*.rs` files —
`cargo nextest` runs each in its own process by default.

### Test categories

1. **Unit tests** (in-crate, no `install()`):
   - Empirical sampling rate matches target within ±10% over ≥10K
     simulated allocs.
   - `draw_exponential` distribution sanity (mean, variance).
   - Deterministic via `rng_seed`.
2. **Integration** (separate `tests/` files):
   - Alloc a known pattern, inspect the trace for expected events and
     approximate counts.
   - Verify `AllocEvent.tid` matches the allocating thread.
   - Verify stacks whose top frame matches the allocation callsite.
3. **Liveset** round-trip:
   - Alloc N, free M, verify liveset.len() == N - M.
   - Verify `FreeEvent.size` and `FreeEvent.alloc_timestamp_ns` match
     the original `AllocEvent`.
4. **Reentrancy:**
   - Record a tracing event whose subscriber allocates; confirm no
     infinite recursion.
5. **Realloc:**
   - Alloc, realloc to larger (in-place and moved), verify free-of-old
     + alloc-of-new per jemalloc rules.
6. **Rotation robustness:**
   - Allocate long-lived buffer, trigger rotation so `AllocEvent` is
     evicted, free the buffer, verify `FreeEvent.size` is non-zero.
7. **Concurrency (shuttle):**
   - Two threads sampling simultaneously, a thread freeing while
     another inserts, epoch advancement under contention.

---

## 12. Open questions

1. **Per-alloc allocator latency.** Wrapping `self.0.alloc` with a
   `clock_monotonic_ns` pair gives allocator latency for
   fragmentation investigations. Cost: +50 ns per sampled alloc. Easy
   to add as a field later with a lower sample rate (e.g. 4 MiB).

2. **MUSL / static builds.** The `safe_load` trampoline + SIGSEGV chain
   needs verification on musl. If unreliable, conditionally compile out
   the frame-pointer unwinder on musl targets and fall back to
   no-stack-capture mode.

3. **Multi-runtime selection.** With a single captured handle, all
   allocations land in one trace. If a service runs multiple dial9
   runtimes and wants per-runtime attribution, we'd need a different
   strategy. Punt until the use case is real.

4. **Dynamic ring-buffer sizing.** Today `ring_capacity` is fixed at
   install time. If sustained bursts exhaust the ring, the user bumps
   `ring_capacity` after seeing dropped-samples > 0. Auto-resize or
   per-thread overflow buffers are possible but not needed at default
   sample rates.

---

## 13. Known limitations

1. **`MAX_FRAME_SIZE` bias.** The unwinder stops walking if
   `saved_fp - fp > 256 KiB` (rejects wild pointers). For allocation
   profiling this bias is systematic — allocations inside functions
   with unusually large stack frames (large `Box::pin(future)` state
   machines, large `[u8; N]` locals) will consistently have truncated
   stacks. Plan: raise to 1 MiB with empirical validation of
   false-frame rate. Longer term, a variable-size ring buffer would
   allow capturing full stacks without a fixed frame cap.

2. **SIGSEGV handler ordering.** Applications that install their own
   SIGSEGV handler after dial9 initialization may break the unwinder's
   fault tolerance.

3. **Dealloc contention at extreme rates.** The free queue MPMC
   saturates at ~2.7 M pushes/sec aggregate. Services with 10M+
   deallocs/sec need a producer-side optimization (per-thread batch
   or bloom filter). Liveset is off by default for this reason.
