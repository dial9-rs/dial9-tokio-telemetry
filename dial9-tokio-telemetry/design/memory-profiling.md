# Memory Profiling

## Overview

Add sampled allocation profiling to dial9. A `Dial9Allocator<A>` wraps any
`GlobalAlloc` (default `System`) and emits `AllocEvent` / `FreeEvent` events
into the trace on sampled allocations. Stacks are captured via the frame
pointer unwinder we already use for CPU profiling. The viewer and analysis
toolkit gain allocation flamegraphs, per-task allocation totals, and leak
detection.

The design mirrors jemalloc's and Go's allocation profilers: **geometric
(Poisson) sampling** keyed on allocation size. The expected sampling rate
is 1 sample per N bytes allocated, regardless of object size distribution.
This gives unbiased size-weighted profiles at bounded overhead.

## Goals

- Always-on in production, with sub-1% overhead at default sampling rates.
- Works with any `GlobalAlloc` (system, jemalloc, mimalloc, etc.) via a
  zero-cost wrapper.
- Stacks tied to the worker thread + task that performed the allocation, so
  allocation hot paths can be attributed to specific tasks or poll ranges.
- Optional live-set tracking for leak detection. Off by default — it adds
  a per-allocation hashmap lookup and a concurrent hashmap.
- No symbolication on the hot path. Addresses flow through the existing
  background `SymbolizeProcessor`.
- **Install-once, captured handle.** `MemoryProfiler::install(handle)`
  sets a process-global static that the allocator reads. The captured
  handle routes all sampled events through the central recorder —
  allocations on any thread (tokio workers, blocking pool, user OS
  threads, library-spawned threads) get recorded identically.

## Non-goals

- Tracking every allocation. We sample.
- Replacing heaptrack / valgrind. Those record every allocation and are
  fine for dev-time analysis. Dial9 is for production, always-on.
- Detecting use-after-free, double-free, or other memory safety bugs.
  That's what Miri and ASAN are for.
- Tracking stack allocations or `mmap`-backed memory outside the
  allocator. A future `RssSample` event could sample `/proc/self/statm`,
  but that's out of scope here.

---

## 1. Geometric sampling

Per-thread byte counter `next_sample_bytes`. On every allocation of size
`s`:

```rust
fn on_alloc(size: usize) {
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

Yes — this matches the structure you sketched. Two notes on that sketch:

- The `while` loop is important. A single huge allocation (or a very low
  sample rate) can push `next_sample_bytes` below zero by more than one
  draw's worth. We need to keep drawing until the counter is positive
  again so we don't sample the *next* allocation immediately too.
- `remaining` has to be `i64` (or signed `isize`). Subtracting `usize`
  from a `usize` and comparing to zero invites wraparound bugs.

Default `sample_rate_bytes = 512 KiB`. At that rate, a service doing 1 GB/s
of allocation generates ~2000 samples/sec — plenty of signal, trivial
overhead.

### RNG

**Per-thread RNG in TLS, not process-global.** Sampling decisions run
on the allocation hot path — even the *unsampled* path hits this RNG
once per allocation (to decrement the per-thread sample counter and
check whether it tripped). A shared `AtomicU64` would force a
read-modify-write across threads on every allocation; TLS gives each
thread its own state with a plain load/store.

```rust
thread_local! {
    static SAMPLE_RNG: Cell<Xoshiro256PlusPlus> = Cell::new(
        Xoshiro256PlusPlus::seed_from_u64(global_seed().wrapping_add(thread_nonce()))
    );
}
```

Each thread seeds its state lazily on first sample from a shared
install-time seed mixed with a per-thread nonce (e.g.
`ThreadId::as_u64()` or a counter), so the stream is deterministic
given `rng_seed` and reproducible across runs for tests.

Short-lived threads that never sample pay zero: `thread_local!`
initializers don't run until the first access, so a blocking-pool
thread that never allocates enough to trip the sampler never
materializes a generator.

The draw itself: `-ln(uniform_0_1()) * R`. We use the fast-log2 trick
that Go uses (`fastexprand` in `runtime/malloc.go`): draw a random
integer from `[1, 2^26]`, take `fastlog2` of it, multiply by `-ln(2) * R`,
add 1. Single-digit ns on modern CPUs, no FP library call.

### Why geometric over alternatives

Unbiased estimates of total bytes allocated per call site. Reservoir
sampling biases against allocations that occur late in a program's
lifetime. Fixed N-of-every-M biases against large objects that get
undersampled. Jemalloc's profiling internals doc (referenced in the
doc_internal tree) has the full variance analysis — short version: per-byte
Bernoulli sampling via the geometric/exponential trick gives the lowest
variance estimator of the simple strategies.

### Small-object unbiasing

When you sample at rate `R` and see an allocation of size `s < R`, the
*expected* size represented is `R / (1 - exp(-s/R))`. The analysis toolkit
applies this to produce unbiased byte totals. Jemalloc uses the same
formula (`jeprof` divides by `1 - exp(-Z/R)`). Aggregation must happen
*after* unbiasing per sample — sum-then-unbias underreports small-object
stacks. We'll document this for the toolkit.

---

## 2. Stack capture via `perf-self-profile`

The CPU profiler's frame-pointer unwinder (`fp_profiler::unwind::unwind`)
is exactly what we need:
- No allocations.
- Safe against corrupted frame chains (the `safe_load` SIGSEGV handler
  is already installed by `install_handler()`).
- **~5 ns per frame, ~110 ns for a 20-frame walk** on x86_64 (measured
  on AMD EPYC 9R14 via `perf-self-profile/benches/unwind.rs` with
  `-C force-frame-pointers=yes`; 27-frame walk = 146 ns mean, 12-frame
  walk = 66 ns mean). Add ~50–200 ns in production for cold caches;
  faulting frames that hit the SIGSEGV safe-load path cost ~1–5 µs
  for the single faulting frame.

`fp_profiler` is currently `pub(crate)`. We need to expose a public API.

### Proposed public `unwinder` module in `dial9-perf-self-profile`

```rust
// dial9-perf-self-profile/src/lib.rs
pub mod unwinder;
```

```rust
// dial9-perf-self-profile/src/unwinder.rs

/// Handle that proves the SIGSEGV fault handler is installed.
/// Zero-sized, freely copyable.
#[derive(Clone, Copy, Debug)]
pub struct Unwinder { _private: () }

impl Unwinder {
    /// Install the SIGSEGV fault handler used by stack capture.
    /// Idempotent: safe to call multiple times from multiple threads.
    ///
    /// Returns `Err` if `sigaction` fails.
    ///
    /// # Requirements
    /// - Frame pointers (build with `-C force-frame-pointers=yes`).
    pub fn install() -> std::io::Result<Self>;

    /// Capture a stack trace of the calling thread into `out`. Returns
    /// the number of frames written. Never allocates.
    ///
    /// # Frame-0 contract
    /// `out[0]` is the return address *into the caller of `capture`* —
    /// i.e. the PC where `capture` itself will return. Subsequent frames
    /// walk outward via the frame-pointer chain. Callers should expect
    /// to skip `capture` itself plus any `#[inline(never)]` shim they
    /// insert.
    ///
    /// In particular: if `capture` is called from a helper
    /// `on_alloc_sampled()` that is in turn called from `GlobalAlloc::alloc`,
    /// frame 0 will be inside `on_alloc_sampled`, frame 1 inside
    /// `GlobalAlloc::alloc`, frame 2 at the user allocation site. The
    /// analysis toolkit symbolizes all frames and the viewer presents
    /// them unmodified; UI-level "skip frames for clarity" stays a UI
    /// decision, not a capture-time one.
    ///
    /// # Safety contract
    /// Must not be called from inside a different SIGSEGV handler.
    pub fn capture(&self, out: &mut [u64]) -> usize;
}
```

**Known bias: `MAX_FRAME_SIZE`.** The underlying unwinder stops walking
if `saved_fp - fp > 256 KiB` (this cap rejects wild pointers that happen
to be above `fp` but aren't real frames). For CPU profiling the
occasional truncation caused by a large on-stack future or struct is
rare enough to ignore; for *allocation* profiling the bias is
systematic — allocations performed inside functions with unusually
large stack frames (large `Box::pin(future)` state machines, large
`[u8; N]` locals) will consistently have their stacks cut off at the
big frame. We accept this for the MVP and document it as a known
limitation. Raising the cap or making it configurable in the unwinder
is future work.

**Why a handle-returning `install`, not auto-install-on-first-capture?**

1. Install happens once, at a point the caller chooses. No hidden
   first-call latency spike buried inside `capture`.
2. Install can fail (`sigaction` → `EINVAL` on weird kernels, or a
   constructor conflict). A `Result` at install time surfaces this
   cleanly; a fallible `capture` would have to decide between silently
   returning 0 or propagating an error on the hot path — neither is
   good.
3. `&self` gives `capture` a natural place to hang per-unwinder state
   later (thread-local caches, config) without a breaking API change.
4. The "handler installed" check disappears from the hot path. Holding
   an `Unwinder` is the proof.

Internally `capture` does:
1. Read `pc`, `fp`, `sp` of the current frame via inline asm.
2. Call the existing `unwind(pc, fp, sp, out)`.

The current `unwind_from_ucontext` path stays internal — it's only
needed from inside a signal handler, which external callers shouldn't
be doing.

`MemoryProfiler::build` calls `Unwinder::install()` and stores the
returned handle. The hook accesses it via the process-global
`MemoryProfilerInner` (§7):

```rust
struct MemoryProfilerInner {
    unwinder: Unwinder,
    ...
}

// In the hook:
// 128 frames × 8 B = 1 KiB stack buffer. Rust async call stacks routinely
// exceed 40 frames (state machines, tower layers, hyper service stacks,
// futures::join_all), so 64 is on the edge; 128 gives comfortable
// headroom without meaningful stack pressure.
let mut frames = [0u64; 128];
let n = inner.unwinder.capture(&mut frames);
```

### SIGSEGV handler lifecycle

The handler is permanently installed after `Unwinder::install()`.
It is **not** uninstalled when capture completes. That's safe and
intentional:

- The divert-and-return logic fires *only* when the faulting PC falls
  within the `safe_load_start..safe_load_end` code range. Those
  instructions execute only inside the `unwind` walk. Everywhere else
  in the process, a SIGSEGV has a PC outside that range and the
  handler chains to the previously-installed one (or to `SIG_DFL`,
  which terminates the process as expected).
- "Uninstall between captures" would just be
  `sigaction(SIGSEGV, old, null)` and `sigaction(SIGSEGV, ours, old)`
  around every `capture`. That's two syscalls per stack walk
  (hundreds of ns each) — an order of magnitude more expensive than
  the ~110 ns walk itself — for no correctness gain.

**Known limitation:** if the application installs its own SIGSEGV
handler *after* dial9 initializes, it will not chain back to ours,
and `safe_load` loses its fault tolerance. This is a pre-existing
issue with CPU profiling and applies identically here. Applications
that install their own SIGSEGV handlers should do so before
initializing dial9.

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

**Where does the sampling rate live?** Not on the event. The sample
rate is **immutable for the life of a trace file** and is written
into `SegmentMetadata` (the same mechanism used by `RuntimeName`,
`BootId`, etc.):

```
SegmentMetadata["memory_profile.sample_rate_bytes"] = "524288"
```

The analysis toolkit reads `sample_rate_bytes` from segment metadata
and applies it uniformly to every `AllocEvent` in that segment when
computing unbiased byte totals. Per-event storage of the weight
would add 1–9 LEB128 bytes to every sampled allocation for data
that never varies within a file — wasteful at steady-state trace
rates.

**Can the rate change at runtime?** Not within a single trace file.
If we later want a `set_sample_rate_bytes` API, it will:

1. Atomically update the rate held in `MemoryProfilerInner`.
2. Force a segment rotation so the new segment's `SegmentMetadata`
   carries the new rate.
3. Existing per-thread `next_sample_bytes` counters drain naturally
   under the old rate; subsequent redraws use the new rate. The
   tiny post-change bias (at most one gap's worth per thread using
   the old rate) is negligible.

For the MVP, the rate is set once at `install()` and never changes.

**Why explicit `tid`?** A trace can contain allocations from threads
that aren't currently inside a poll — blocking-pool workers,
user-spawned OS threads, early-boot allocations before any runtime
exists. `CpuSample` already carries `tid` for the same reason; we
match, so alloc events and CPU samples join cleanly on thread.

We **don't** carry a `worker_id`: when the alloc happens on a tokio
worker, the worker's identity is recoverable by joining `tid` to the
most recent `WorkerUnpark` for that thread (analyzer walks the
`WorkerPark`/`WorkerUnpark` stream it already parses). Keeping the
field off the event shaves 1 byte per sample in the common case and
avoids inventing a sentinel for "not on a worker." `ThreadNameDef
{ tid, name }` already exists in the format (emitted by the CPU
profiler) and does double-duty for alloc events — no new metadata
needed.

### `FreeEvent` (liveset only)

```rust
#[derive(TraceEvent)]
struct FreeEvent {
    #[traceevent(timestamp)]
    timestamp_ns: u64,
    /// Pointer that was freed. Matches a previously-seen `AllocEvent.addr`.
    addr: u64,
    /// Size of the allocation being freed. Denormalized from the
    /// matching `AllocEvent` so the free stays analytically useful
    /// when the corresponding `AllocEvent` has been evicted by trace
    /// rotation.
    size: u64,
    /// Monotonic-ns timestamp of the original `AllocEvent`. Allows
    /// leak analysis to bucket frees by generation without needing
    /// the `AllocEvent` in the same (unrotated) trace.
    alloc_timestamp_ns: u64,
}
```

**Why denormalize `size` and `alloc_timestamp_ns`?** `RotatingWriter`'s
default is 60-second segments with a total-size eviction budget. A
long-lived allocation (hours of cache residency) will have its
`AllocEvent` in a segment that has been evicted by the time its
`FreeEvent` is written. A free that carries only `addr` is then
unjoinable — no size to subtract from live-heap totals, no
timestamp to bucket by generation. Denormalizing keeps `FreeEvent`
self-sufficient for net-bytes-freed and generational leak analysis
across rotation.

The hot-path cost is zero: the liveset entry we look up to decide
whether to emit the free already carries both fields. Wire cost is
a few bytes per `FreeEvent` (LEB128 varint encoding).

We do **not** denormalize the allocation stack onto `FreeEvent`:
storing the full stack in every liveset entry would bloat the liveset
~8× (64 × 8 B per stack vs the ~16 B size+timestamp pair), and that
memory is paid *while the allocation is live*. Stack-attributed leak
analysis is still possible inside a single retained segment via the
`AllocEvent↔FreeEvent` join on `addr`.

`FreeEvent` has no `worker_id` — a free is memory-address-bound, not
thread-bound for analysis purposes. If "which thread freed it" becomes
interesting we can add it later.

### `realloc` handling

What other tools do:

**Go runtime**: treats realloc as a normal `malloc` of the new size
plus a `free` of the old pointer. Both sides are subject to independent
sampling. (`mallocgc` handles reallocation in the user-visible
`append`/`growslice` paths; the profile sees it as two events.)

**jemalloc `prof_realloc`**: 4-way combination based on which sides
were sampled:
- Old sampled + new sampled: emit free-of-old, emit alloc-of-new.
- Old unsampled + new sampled: emit alloc-of-new only.
- Old sampled + new unsampled: emit free-of-old only.
- Neither sampled: emit nothing.

Crucially, jemalloc treats the new side as a *fresh* sampling decision
even when `realloc` doesn't move the pointer. Same as a new `malloc`.

**Our plan: follow jemalloc.** Treat `realloc(p, n_bytes)` as:
1. `dealloc(p, old_layout)` — may emit `FreeEvent` if `p` was sampled
   (and liveset tracking is on).
2. `alloc(new_layout)` — fresh sampling decision, may emit `AllocEvent`.

This is symmetric, simple, and matches existing tooling conventions.
In-place realloc (pointer unchanged) still goes through the same flow;
we don't try to be clever about it. That means in rare cases we might
emit `FreeEvent { addr: p }` followed by `AllocEvent { addr: p }` for
the same pointer — fine, the timestamps disambiguate.

The `Dial9Allocator::realloc` impl delegates to
`self.0.realloc(ptr, old_layout, new_size)` and runs the
alloc/dealloc hook logic around it.

### Worker / task attribution for polls

Both events carry an implicit task_id via the shared `PollStart` context
the flush pipeline already builds. We do **not** add an explicit
`task_id` field because:
1. Every alloc inside a poll already falls between that worker's most
   recent `PollStart` and the matching `PollEnd`.
2. The analysis toolkit already uses this range-matching for CPU samples
   (see `dial9-viewer/skills/analyze.js`).

Allocations outside any poll — from non-worker threads, or on worker
threads that aren't currently polling — carry only `tid` and get no
task attribution. The viewer joins `tid` against the runtime's
`WorkerUnpark` history to decide whether the allocation was on a
tokio worker at that instant, and shows out-of-poll allocations in a
"blocking" or "unknown" lane when not.

---

## 4. `Dial9Allocator<A>`

Generic wrapper, default `A = System`:

```rust
pub struct Dial9Allocator<A = std::alloc::System>(A);

impl Dial9Allocator {
    /// Wrap the system allocator. Use this when you don't need a
    /// custom inner allocator (i.e. you weren't otherwise setting
    /// `#[global_allocator]`).
    pub const fn system() -> Self { Self(std::alloc::System) }
}

impl<A: GlobalAlloc> Dial9Allocator<A> {
    /// Wrap a custom allocator (e.g. jemalloc, mimalloc).
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
        hook::on_dealloc(ptr, old_layout.size());
        let new_ptr = unsafe { self.0.realloc(ptr, old_layout, new_size) };
        if !new_ptr.is_null() {
            hook::on_alloc(new_ptr, new_size);
        }
        new_ptr
    }

    // alloc_zeroed delegates similarly.
}
```

User code:

```rust
// Common case: system allocator. No turbofish needed.
#[global_allocator]
static ALLOC: Dial9Allocator = Dial9Allocator::system();

// Wrapping a custom allocator:
#[global_allocator]
static ALLOC: Dial9Allocator<tikv_jemallocator::Jemalloc> =
    Dial9Allocator::new(tikv_jemallocator::Jemalloc);
```

The wrapper is zero-cost when memory profiling isn't attached (see §6).

---

## 5. Ring-buffer hand-off (considered, rejected for MVP)

Your idea: have the allocator hook push raw data to a ring buffer, then
a separate thread reads and processes it. This is worth writing out
because it surfaces which costs we can move off the hot path and which
we can't.

**What the allocator hook does:**
1. Sampling decision. Per-thread counter, ~5ns.
2. Stack capture. ~110 ns for a 20-frame stack (measured, warm-cache;
   see §2). Add cold-cache overhead in production.
3. Encode event. ~100-200ns.
4. Maybe insert into liveset hashmap. ~30-50ns.

**The problem:** stack capture has to happen on the allocating thread
in-place. By the time a reader thread runs, the frames the allocator
returned through have already been popped — the stack we'd want to
walk is gone. So we can't defer (2) to a reader.

**What a ring buffer would let us defer:** (3) encoding, and (4)
liveset insertion.

Encoding costs ~100-200ns of the ~1-2µs sampled path. Deferring it
moves about 5-10% of sampled-path cost off the allocating thread.
That's real, but small relative to stack capture.

**The big win — if it works:** eliminating encoding from the hot path
also eliminates the reentrancy hazard (§6). `intern_string`,
`HashMap::insert`, `Vec::grow` — all of those happen on the reader,
which isn't itself traced. This is actually the better argument for
the ring buffer design.

**The catch:** the ring buffer entries need to store the captured
stack (N × u64 addresses) plus size, timestamp, `tid`. That's
~150-250 bytes per sample. A lock-free SPSC queue per thread sized at
256 entries = ~50 KiB per thread. Doable but non-trivial memory.

**Decision for MVP: don't build the ring buffer.** Use a thread-local
reentrancy guard (§6) and the existing thread-local buffer
infrastructure. Revisit if:
- The reentrancy guard turns out to drop too many events (we'd see
  this in a "dropped alloc events" counter we add from day one).
- Encoding on the hot path shows up in profiles.
- The simpler design has any correctness issue we can't solve
  otherwise.

The ring buffer lives in §13 (future work) as "Phase 2."

---

## 6. Reentrancy — the central hazard

A global allocator sees *every* allocation in the process, including
allocations performed by dial9 itself while recording an event. Without
a guard, we recurse into `on_alloc` from inside `on_alloc` and either
deadlock or blow the stack.

Concrete cases:
- `Encoder::intern_string` calls `s.to_string()` and `HashMap::insert`
  — both allocate.
- Growing the thread-local buffer's `Vec<u8>` allocates.
- The liveset `DashMap::insert` allocates.
- tracing logs allocate.

### Guard: thread-local "I'm recording an alloc event" flag

```rust
thread_local! {
    static IN_ALLOC_HOOK: Cell<bool> = const { Cell::new(false) };
}

fn on_alloc(ptr: *mut u8, size: usize) {
    if IN_ALLOC_HOOK.with(|f| f.replace(true)) {
        return; // reentrant — skip
    }
    // ... sampling decision, stack capture, event emission ...
    IN_ALLOC_HOOK.with(|f| f.set(false));
}
```

The flag is checked with a single relaxed TLS load.

**Consequence:** we don't see dial9's own internal allocations in the
profile. 

### Consequence: dropped events

Sampled allocations that happen *during* another sampled allocation's
event recording are silently dropped. In practice: the probability of
a sampled alloc (1-in-R) occurring during another sampled alloc's
~1-2µs processing is `1e-6 * rate_per_sec * processing_time_sec` —
negligible (<0.001% drop rate at 2000 samples/sec). We'll add a
`dropped_samples` counter to the `dial9_stats` event so operators can
confirm.

**Interaction with the `tracing-layer` feature.** The guard drops
*any* recording work that happens inside a sampled allocation's stack
capture + encoding, not just dial9's internal allocations. If a user
has both `tracing-layer` and memory profiling enabled and a
`tracing::span!` enter/exit happens to land on a thread that is
currently ~1 µs deep in `on_alloc`'s hot path (stack capture +
encode), that span event is dropped. At 2000 samples/sec and typical
tracing-span rates the collision rate is still tiny (<0.01%), but
this is a real and non-obvious behavior that operators reviewing
dropped-event counters need to know about. Noted here because it's
the kind of interaction that looks like a bug a year from now.

### Allocation during stack capture

`capture()` must never allocate. We enforce this by only using
stack-resident buffers (`[u64; MAX_FRAMES]`) and calling into the
existing fp-based unwinder, which is allocation-free.

### Early-boot allocations

Allocations happen before `main` runs (`static` initializers, argv
parsing). At that point no `MemoryProfiler` exists. The hook checks
an uninitialized `OnceLock<MemoryProfilerState>` (see §7) and returns
early. ~2ns overhead per early-boot alloc.

---

## 7. Configuration — static install, captured handle

`MemoryProfiler::install()` sets a process-global static that the
`Dial9Allocator` reads on every allocation. Installation can happen
**exactly once per process** and takes a `TelemetryHandle` captured
at install time. The hook uses that captured handle directly —
no `TelemetryHandle::current()` lookup on the hot path.

This works for any thread, not just tokio workers. When a non-tokio
thread performs its first sampled allocation, the
`record_encodable_event` path inside the handle registers that
thread's `TlBufferHandle` with the central collector, exactly the
same way `tracing_layer` or custom events get registered today. A
random `std::thread::spawn`ed thread that allocates → events flow
through the central collector → trace file.

### Shape

```rust
use dial9_tokio_telemetry::memory_profiling::{
    Dial9Allocator, MemoryProfiler, TimestampMode,
};

// Install the global allocator (static, runs before main).
#[global_allocator]
static ALLOC: Dial9Allocator = Dial9Allocator::system();

fn main() {
    // 1. Build the runtime as usual.
    let guard = TelemetryCore::builder()
        .writer(writer)
        .trace_path("/tmp/trace.bin")
        .build()?;
    guard.enable();

    // 2. Install the memory profiler with the live handle.
    //    Returns Err(AlreadyInstalled) if called a second time.
    let _mem = MemoryProfiler::builder()
        .sample_rate_bytes(512 * 1024)
        .track_liveset(true)
        .timestamp_mode(TimestampMode::ReusePollStart)
        .install(guard.handle())?;

    // 3. Build + attach a runtime. Allocations on every thread —
    //    including non-tokio threads — are captured.
    let (rt, _) = guard.trace_runtime("main").build(rt_builder)?;
    rt.block_on(async { /* ... */ });
}
```

Between steps 1 and 2 (after the global allocator static is installed
but before the profiler is configured), every allocation takes the
unset-`OnceLock` fast path: one `Acquire` load + null check (~1ns),
not set → skip. No events, no crashes, no setup order to get right.

### Why capture the handle, not look it up per-alloc

The `Dial9TokioLayer` calls `TelemetryHandle::current()` on each
event because tracing spans run on arbitrary threads and the layer
needs to discover whether the *current* thread belongs to a dial9
runtime. The layer has no concept of "the" runtime — if multiple
runtimes run concurrently, each thread's events flow into its own
runtime's trace.

For memory profiling, that's the wrong semantics. There's one global
allocator, allocations come from everywhere, and we want *all* of
them (from tokio threads, OS-level threads, the allocator inside a
library's background thread) to land in the same trace. A single
captured handle achieves that.

Incidental benefit: skipping the `current()` TLS lookup saves a few
ns per sampled alloc. Not the reason, but nice.

### `MemoryProfiler::install()` flow

```rust
static ACTIVE: OnceLock<MemoryProfilerState> = OnceLock::new();

impl MemoryProfiler {
    pub fn install(
        self,
        handle: TelemetryHandle,
    ) -> Result<MemoryProfilerGuard, InstallError> {
        // 1. Install SIGSEGV handler, get Unwinder.
        let unwinder = Unwinder::install().map_err(InstallError::Unwinder)?;

        // 2. Build the state.
        let state = MemoryProfilerState {
            unwinder,
            handle,
            config: self.config,
            liveset: self.config.track_liveset.then(DashMap::new),
        };

        // 3. Publish exactly once. `OnceLock::set` returns `Err(state)`
        //    on a second call, which maps directly to AlreadyInstalled.
        ACTIVE
            .set(state)
            .map_err(|_| InstallError::AlreadyInstalled)?;

        Ok(MemoryProfilerGuard { _private: () })
    }
}

#[derive(Debug)]
pub enum InstallError {
    AlreadyInstalled,
    Unwinder(std::io::Error),
}

/// Dropping this guard does NOT uninstall the profiler. The static
/// state lives until process exit. The guard exists to make the
/// API familiar (RAII) and to hold a lifetime if we add a pause/
/// resume later.
pub struct MemoryProfilerGuard { _private: () }
```

The state lives in a `OnceLock<MemoryProfilerState>` and is never
reclaimed because:

1. In-flight hook calls may be reading `ACTIVE.get().unwrap().handle`
   at any moment on any thread. Freeing while a reader holds the
   reference is unsound. We'd need hazard pointers or RCU to free
   safely.
2. The only time we'd want to free is at process exit, when the OS
   reclaims everything anyway.

Cost is ~100 bytes plus the liveset size. Acceptable.

**Why `OnceLock` instead of a raw `AtomicPtr`.** The semantics we
need are exactly "write once, read many, never reclaim," which is
what `OnceLock` encodes in the type system — no `unsafe`, no
pointer-provenance footguns, and the no-uninstall invariant is
enforced by the absence of a safe `take()`-equivalent rather than
by a comment. Hot-path cost is identical: `OnceLock::get()` on a
set value is an `Acquire` load of an internal pointer slot plus a
null check, the same thing the hand-rolled `AtomicPtr::load` would
compile to.

### Hot path

```rust
fn alloc(&self, layout: Layout) -> *mut u8 {
    // SAFETY: forwarding to the inner allocator.
    let ptr = unsafe { self.0.alloc(layout) };
    if !ptr.is_null() {
        if let Some(state) = ACTIVE.get() {
            hook::on_alloc(state, ptr, layout.size());
        }
    }
    ptr
}
```

Inside `hook::on_alloc`, after the reentrancy guard, sampling
decision, and stack capture:

```rust
fn emit_alloc_event(
    state: &MemoryProfilerState,
    size: usize,
    stack: &[u64],
) {
    // The captured handle is always enabled (would not have been
    // passed to install() otherwise). record_event handles the
    // first-time-on-this-thread registration with the central
    // collector internally.
    record_event(
        AllocEventWire { /* ... */, stack: state.intern(stack) },
        &state.handle,
    );
}
```

Note the absence of `TelemetryHandle::current()`. `state.handle` is
the handle the user passed into `install()`.

### What about `disabled` handles?

`install(handle)` accepts any `TelemetryHandle`, including an inert
one. If the user installs with a disabled handle (e.g. because
`Dial9Config::builder().build_or_disabled()` produced a disabled
runtime after an I/O error), the hot path still runs the sampling
decision + stack capture — wasted work. We could short-circuit by
checking `handle.is_enabled()` at install time and skipping the
`ACTIVE.store` if not. Lean: do this. Single check at install time,
keeps the "did the user enable memory profiling?" / "does dial9
actually have a live trace?" conditions aligned.

### `MemoryProfilingConfig`

```rust
#[derive(Debug, Clone)]
pub struct MemoryProfilingConfig {
    sample_rate_bytes: u64,             // default 512 KiB
    track_liveset: bool,                // default false (off)
    timestamp_mode: TimestampMode,      // default ReusePollStart
    max_liveset_entries: Option<usize>, // default None (unbounded)
    rng_seed: Option<u64>,              // test-only; seeds per-thread RNGs deterministically
}

/// How `AllocEvent.timestamp_ns` is populated.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[non_exhaustive]
pub enum TimestampMode {
    /// Reuse the timestamp stamped into TLS by the most recent
    /// `PollStart` on this thread when one is available
    /// (~2 ns TLS load). On threads that have no recorded `PollStart`
    /// yet — blocking-pool workers, user OS threads, library-spawned
    /// threads, or a tokio worker pre-first-poll — fall back to
    /// `clock_monotonic_ns()` (~25 ns vDSO).
    ///
    /// All allocations within the same poll get the same timestamp;
    /// ordering within a poll is lost but analysis still has a usable
    /// time for every event.
    ///
    /// Default.
    ReusePollStart,

    /// Emit events with `timestamp_ns = 0`. Smallest on-disk size
    /// (LEB128-encoded zero is 1 byte). Analysis toolkit can still
    /// group by stack and size; viewer loses time-range filtering and
    /// cross-event correlation.
    ///
    /// Useful when you only care about aggregate hot stacks and want
    /// minimal overhead + trace size.
    None,

    /// Call `clock_monotonic_ns()` per sampled allocation (~25 ns via
    /// vDSO on Linux). Only sampled allocations pay this; unsampled
    /// ones still take the fast path.
    ///
    /// Use this when investigating tight allocation loops or timing
    /// correlation with other events within a single poll.
    Precise,
}

impl MemoryProfiler {
    pub fn builder() -> MemoryProfilerBuilder { ... }
}
```

Built via `#[bon::builder]` as with other dial9 configs.

### Why an enum over a bool for timestamps

Three real modes, not two:
- `ReusePollStart` is the right default — cheap, good enough for
  flamegraph and per-poll analysis.
- `Precise` for tight-loop investigations.
- `None` is actually useful: minimum overhead + minimum trace size
  for customers who just want aggregate stack hotness.

An enum also leaves room to add modes without breaking existing calls
(e.g. `Coarse` at 10ms resolution if profiling shows vDSO overhead
under extreme load). `#[non_exhaustive]` protects the semver.

---

## 8. Liveset tracking

When `track_liveset = true`, we maintain:

```rust
struct LivesetEntry {
    /// Copied onto `FreeEvent.size` on dealloc.
    size: u64,
    /// Copied onto `FreeEvent.alloc_timestamp_ns` on dealloc. This is
    /// the `AllocEvent.timestamp_ns` we emitted for the matching alloc.
    timestamp_ns: u64,
    // No stack frames here. The per-live-alloc memory cost of storing
    // a full call stack (64 × 8 B) dominates total liveset overhead.
    // Stack-attributed leak analysis joins FreeEvent→AllocEvent by
    // `addr` inside a single retained segment instead.
}

// Shared, concurrent: all threads alloc; all threads free.
// Lives inside MemoryProfilerInner.
liveset: DashMap<usize, LivesetEntry>,
```

### Why `DashMap`?

Requirements:
- `insert(addr, entry)` on alloc from any thread.
- `remove(addr) -> Option<entry>` on dealloc from any thread.
- Low contention on mixed read/write workload.
- `Send + Sync`.

Options evaluated:

| Crate | Pros | Cons |
|-------|------|------|
| `dashmap` | Widely used, sharded, good perf | Allocates internally; not lock-free |
| `scc::HashMap` | Lock-free, fast | Larger dep surface, newer |
| `std::sync::Mutex<HashMap>` | Zero deps | Single lock, bad under contention |
| Sharded `Mutex<HashMap>` by addr hash | Simple, no new deps | We'd hand-roll what dashmap gives us |

Start with `dashmap`. Abstract behind a `trait Liveset` so swapping to
`scc` or a sharded-mutex implementation is mechanical if benchmarks
demand it.

**Allocations by the DashMap itself** are caught by the
`IN_ALLOC_HOOK` guard — they go to the inner allocator without
recursing.

### Bounded liveset

Unbounded liveset can consume significant memory in a long-running
leaky process (which is, ironically, when you want it on).
`max_liveset_entries` caps the total count; when full, the overflow
policy is:
- New sampled allocations: still emit `AllocEvent`, but skip liveset
  insert. Emit a rate-limited warning event (once per 60s).
- Deallocations: still lookup (miss on unsampled, or miss on skipped),
  no-op on miss.

Traces stay useful for hotspot analysis even if the liveset overflows.
Leak analysis loses fidelity in that range.

Default: `None` (unbounded). Users opt in to a cap.

### Liveset overhead

Per sampled alloc: one hash + one shard lock + insert (~30-50ns
uncontended).

Per **any** dealloc (not just sampled): one hash + one shard lock +
map lookup. If the pointer isn't in the liveset (typical — 99.9% of
deallocs), this is a lock+lookup+miss on a cold key. ~40-60ns on
DashMap.

Two optimizations to consider:

1. **Bloom filter in front of DashMap.** Sampled `alloc` inserts into
   both; `dealloc` checks the Bloom first, only hits the map on a hit.
   At 1% false positive rate, 99% of unsampled deallocs avoid the map
   entirely. Adds complexity; optional.
2. **Address low-bits hash precheck.** Keep a `RoaringBitmap` of low
   32 bits of tracked addresses, checked first. Similar idea, cheaper
   false-positive handling.

Ship without these. Add based on profiling.

---

## 9. Overhead budget

Target: <1% at default settings (512 KiB sample rate, no liveset).

Per-allocation fast path (unsampled, ~99.9% of calls):
- 1 atomic load of `ACTIVE` pointer (~1ns)
- 1 TLS load of `IN_ALLOC_HOOK` (~2ns)
- 1 subtract + compare on per-thread `next_sample_bytes` (~1ns)
- Return

Total: ~5ns added per alloc. For a service doing 1M allocs/sec that's
0.5% CPU. Most services do far fewer.

Per-sampled allocation (~0.1% of calls at 512 KiB):
- Stack capture (~110 ns for 20 frames, measured warm-cache; ~200–400
  ns likely in production with cold caches)
- `record_event` into TL buffer (~100-200ns)
- Optional timestamp call (~25ns)
- Liveset insert if enabled (~30-50ns)

At 2000 samples/sec: well under 1ms/sec CPU, comfortably under budget.

**Dealloc overhead with liveset:** +40-60ns per dealloc → ~5% extra on
a dealloc-heavy workload. This is why liveset is off by default.

### Benchmarking

The existing `scripts/compare_overhead.sh` drives `overhead_bench`, a
TCP echo workload. Echo is allocation-light by design (hits the fast
path on per-connection buffers, steady-state) and will not surface
regressions in the allocation profiling hook.

**We need a new benchmark focused on allocation-heavy workloads**
before we can make meaningful claims about memory-profiling overhead.
Shape of what it should exercise:

- High-frequency small allocations (e.g. tight `Box::new(T)` or
  `Vec::with_capacity(small)` loops) to measure fast-path cost.
- Realloc growth (e.g. `Vec::push` in a loop) to measure the
  free+alloc-sampled path.
- Mixed sizes across the sample-rate boundary so unbiasing has
  variance to work with.
- Comparison matrix: (no profiler) vs (sampling only) vs (sampling +
  liveset).

This bench should land alongside the hook implementation in the same
PR; without it the ~5 ns / ~1 µs estimates in this doc are just
estimates. Design and naming TBD — likely
`benches/memory_profiling_bench.rs` with per-case Criterion groups.

---

## 10. Viewer changes

### Allocation flamegraph

Same UX as CPU flamegraph — click a poll, or shift-drag a time range,
see a flamegraph. The sample value is
`AllocEvent.size * weight_correction(AllocEvent.size, sample_rate)`,
where `sample_rate` is read once per segment from `SegmentMetadata`.
Code lives alongside the existing flamegraph, driven by a new
aggregator that consumes `AllocEvent` instead of `CpuSample`.

### Per-task allocation chart

For each task, total bytes allocated + sampled count. Sort by size;
shows leaky / hot tasks at a glance. Read directly from trace:

```
sample_rate = segment_metadata["memory_profile.sample_rate_bytes"]
for each AllocEvent e:
    worker = worker_for(e.tid, e.timestamp_ns)  // via WorkerUnpark history
    poll = poll_containing(e.timestamp_ns, worker)
    task = poll.task_id
    agg[task] += e.size * weight_correction(e.size, sample_rate)
```

This is the same join the viewer already does between `CpuSample` and
polls. Allocations with no containing poll go into an "unassociated"
bucket — common for background threads, first allocations on new
workers, etc.

### Leak view (liveset only)

Show allocations with no matching free at end-of-trace:
```
live = {}
for each AllocEvent: live[addr] = (size, stack)
for each FreeEvent:  live.remove(addr)
# group by stack, sort by total bytes
```

The `(size, stack)` lookup is cheap: one hashmap entry per live
sampled allocation in the trace, joined as we stream events.

---

## 11. Analysis toolkit

Extend `dial9-viewer/skills/analyze.js` with:

- `allocationsByTask()` — groups by task, weighted.
- `topAllocationStacks(n)` — flamegraph-style stack aggregation,
  unbiased.
- `leakCandidates(minBytes)` — live allocations grouped by stack,
  above a threshold. Only meaningful when the trace has `FreeEvent`s.

CLI:

```bash
cargo run --example analyze_trace --features analysis -- \
    --memory trace.0.bin.gz
```

Emits: per-task alloc totals, top 20 stacks, leak candidates if
available.

New skill doc: `dial9-viewer/skills/memory.md` covering recipes for
common questions ("what allocated the most bytes in this time range?",
"which stacks show the largest retained heap?", etc.).

---

## 12. Testing strategy

**Install-once is permanent per process.** `MemoryProfiler::install`
publishes a process-global `OnceLock<MemoryProfilerState>` that is
never reclaimed (see §7 for why). Tests that exercise different
memory-profiling configurations therefore each need their own
process.

We do **not** add a test-only `reset_for_testing()` escape hatch:

- The soundness reason for no-reclaim (hooks on any thread may be
  reading `ACTIVE.get()` at any moment) applies in tests too. A
  `reset_for_testing()` that doesn't implement hazard pointers is a
  data race; one that does has cost we don't want to pay in prod
  builds.
- `cargo nextest run` already runs each `#[test]` in its own process
  by default, so integration-style tests organized as
  `tests/memory_profiling_*.rs` files pick up a fresh state
  automatically. This is the approach the existing S3 worker tests
  use (see `tests/s3_integration.rs`).
- Contributors who run tests via `cargo test` (which reuses the
  process across tests within a binary) need to structure tests
  within a single file so they share a consistent profiler
  configuration, or move into separate `tests/` files. This is
  called out in the tests' module docs.

### Test categories

1. **Unit tests** around the sampling math (in-crate, do not
   `install()` a profiler):
   - Empirical sampling rate matches target rate within ±10% over
     ≥ 10k simulated allocs.
   - `draw_exponential` distribution sanity (mean, variance).
   - Deterministic via `rng_seed`.
2. **Integration** with a dial9 runtime (each in its own
   `tests/memory_profiling_*.rs` file, installing the global
   allocator per file):
   - Alloc a known pattern, inspect the trace for expected events and
     approximate counts.
   - Verify `AllocEvent.tid` matches the thread the alloc ran on, and
     that joining against `WorkerUnpark` history recovers the correct
     worker when the thread is a tokio worker.
   - Verify FP unwinder produces stacks whose top frame matches the
     allocation callsite.
3. **Liveset** round-trip (own test file):
   - Alloc N things, free M, verify liveset.len() == N - M.
   - Verify `FreeEvent` count matches sampled-alloc count of freed
     pointers.
   - Verify `FreeEvent.size` and `FreeEvent.alloc_timestamp_ns` match
     the original `AllocEvent` values (the rotation-robustness
     contract from §3).
4. **Reentrancy** (own test file):
   - Record a tracing event whose subscriber allocates; confirm no
     infinite recursion and the outer event still reaches the trace.
5. **Realloc** (own test file): alloc, realloc to larger (in-place)
   and larger (moved), verify free-of-old + alloc-of-new are emitted
   per jemalloc rules.
6. **Rotation robustness** (own test file): allocate a long-lived
   buffer, trigger rotation so the `AllocEvent` is evicted, free the
   buffer, verify `FreeEvent.size` is non-zero and analysis can still
   compute the net-bytes delta.

---

## 13. Open questions

1. **Do we want per-alloc allocator latency?** Wrapping `self.0.alloc`
   with a `clock_monotonic_ns` pair gives allocator latency, which is
   interesting for jemalloc/mimalloc fragmentation investigations.
   Cost: +50ns per sampled alloc (two vDSO calls). Easy to add as a
   field later.

2. **MUSL / static builds.** The `safe_load` trampoline + SIGSEGV chain
   works on glibc. Need to verify on musl (Alpine containers). If it
   doesn't, fall back to the `backtrace` crate on init — slower but
   allocation-free if called with a preallocated buffer.

3. **Multi-runtime selection.** With a single captured handle, all
   allocations land in the trace owned by whichever runtime the user
   passed to `install()`. If a service runs multiple dial9 runtimes
   and wants per-runtime allocation attribution, we'd need a
   different strategy (e.g. a handle lookup via a trait the allocator
   could call, or tagging each TLS buffer with a runtime-id). I don't
   think this use case is real yet; punt until it is.

---

## 14. Rollout

1. **`Unwinder::install()` / `capture()` public in `perf-self-profile`.**
   Standalone PR. Tests: install once, capture own stack, verify
   first frame is caller. Verify `install()` is idempotent across
   threads.

2. **`Dial9Allocator<A>` with sampling + `AllocEvent` emission, no
   liveset.** Gate behind a `memory-profiling` feature flag.
   `MemoryProfiler::builder().install(guard.handle())` publishes the
   captured handle via `OnceLock<MemoryProfilerState>`; the hook
   uses it directly. Tests: overhead, approximate sampling rate,
   stacks symbolize correctly, allocations on non-tokio threads are
   captured, second `install()` call returns `AlreadyInstalled`.

3. **Liveset tracking + `FreeEvent`s.** Tests per §12. This is when
   `track_liveset(true)` starts doing something.

4. **Realloc handling per jemalloc rules.** Dedicated tests for the
   four cases.

5. **Viewer: alloc flamegraph.** Parser + UI.

6. **Viewer: per-task chart + leak view.** More UI.

7. **Analysis toolkit + skill docs.**

8. **Allocation-focused benchmark in CI.** Land alongside step 2
   (sampling) and extend in step 3 (liveset). Fail CI on regression.
   See §9 "Benchmarking" — needs to exercise high-frequency small
   allocs, realloc growth, and mixed sizes. Without this in place
   the overhead numbers in this doc are unverified estimates.

Each step ships independently.
