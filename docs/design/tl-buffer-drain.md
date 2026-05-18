# Thread-Local Buffer Drain Design

## Problem

On lightly-loaded systems, thread-local (TL) buffers (1 MB default) take a
long time to fill. Since `RotatingWriter` rotates every ~60 s, events from a
given time window can span multiple trace files. Fully-silent threads (no
events flowing) never flush at all until thread exit.

## Current Approach: Mutex + FlushEpoch

Each `ThreadLocalBuffer` is wrapped in `Arc<Mutex<…>>`. On first use, a
`TlBufferHandle { Weak<Mutex<TLB>>, FlushEpoch }` is registered in
`SharedState`. The flush thread uses a two-tick protocol, split across
consecutive iterations of the 5 ms flush loop:

**Tick N−1 (`bump_drain_epoch`):** Increments the global `drain_epoch`
counter. This signals busy worker threads to self-flush on their next
`record_event` call.

**Tick N (`drain_all_tl_buffers`):** After a ~5 ms grace period:

1. Reads the current `drain_epoch`.
2. Iterates all registered handles.
3. Reads each handle's `FlushEpoch` (relaxed). If it matches the current
   epoch, the owning thread self-flushed during the grace period — **skip**
   (zero contention with busy workers).
4. Otherwise, upgrades the `Weak`, locks the buffer, and flushes pending
   events into the `CentralCollector`.
5. Prunes dead `Weak` handles via `retain`.

On shutdown (`exit = true`), both steps happen in the same tick since there
is no next tick for the grace period.

### Epoch-aware self-flush

In `record_event`, after encoding an event, the worker thread checks whether
the global `drain_epoch` has advanced past its local `FlushEpoch`. If so, it
flushes immediately and stamps the current epoch — even if the 1 MB batch
threshold has not been reached. This means busy threads self-flush
opportunistically on the next event after the epoch bump (tick N−1),
typically within microseconds. By the time the intrusive drain fires (tick
N, ~5 ms later), most busy threads have already self-flushed and their
`FlushEpoch` matches the current epoch — so the flush thread skips them
entirely. The intrusive drain path only needs to lock truly idle/silent
threads that haven't recorded any events since the epoch bump.

### Mutex poisoning

Poisoning requires a panic while the lock is held. The only code that runs
under the lock is the encoder and the `accept_flush` handoff, neither of
which should panic. If a panic did occur, the encoder's internal `Vec<u8>`
could be in a partially-written state, so recovering via `into_inner()` and
continuing to write would produce corrupt data.

Instead, all three lock sites (`record_event`, `drain_to_collector`,
`drain_all_tl_buffers`) treat a poisoned mutex as unrecoverable: they log a
rate-limited `tracing::error!` (at most once per 60 s per call site) and
bail out. Because the mutex stays poisoned, the affected thread silently
stops recording for the rest of its lifetime — clean degradation rather than
corrupt output or cascading panics.

### Drain scheduling

The flush loop asks the `TraceWriter` when to drain via two trait methods:

- **`should_drain(&self) -> bool`**: checked every flush cycle (~5 ms).
  Returns `true` when the writer wants TL buffers drained.
- **`drained(&mut self) -> io::Result<bool>`**: called after the drain +
  flush completes. The writer may rotate the segment, advance a timer, or
  do nothing. Returns `true` if a segment rotation occurred.

`RotatingWriter` tracks a `next_drain_time` field set to
`min(rotation_period, 30s)`. When the drain fires, `drained()` checks
whether a rotation boundary has also been crossed:

- **Rotation due**: rotates the segment (which resets both timers).
- **Periodic drain only**: advances `next_drain_time` without rotating.

This ensures idle/silent threads are drained at least every 30 s even when
rotation is disabled (`single_file()`, `Duration::MAX`).

The flush loop uses a two-state machine (`DrainState::Idle` →
`DrainState::EpochBumped` → `Idle`) to avoid the bug where re-checking
`should_drain()` every cycle (it stays true until `drained()` is called)
would forever reschedule the drain without completing it.

### Performance characteristics

- **Busy workers**: self-flush on epoch advance; never locked by the flush
  thread. The `FlushEpoch` check is a single relaxed atomic load — no
  cache-line contention.
- **Idle workers**: locked briefly (~µs) at each drain interval
  (≤ 30 s). The mutex is almost always uncontended because the owning
  thread is idle.
- **Silent workers**: same as idle — the flush thread locks and drains them.
- **Memory**: one `Arc<AtomicU64>` per thread (the `FlushEpoch`), plus one
  `Weak` pointer per thread in the `SharedState` vec.

## Alternative: Lock-Free Left-Right Buffer

If benchmarks show that the mutex causes measurable contention (e.g., in the
`threadlocal_encode` benchmark), a lock-free Left-Right design avoids
acquiring any lock on the hot path.

### Design

Each thread owns two `ThreadLocalBuffer`s (Left and Right) behind an
`AtomicUsize` index selecting the active side. A per-thread `AtomicU64`
epoch counter tracks in-progress writes:

```
struct DoubleBuffer {
    buffers: [Mutex<ThreadLocalBuffer>; 2],
    active: AtomicUsize,       // 0 or 1
    write_epoch: AtomicU64,    // odd = write in progress, even = idle
}
```

**Writer (hot path):**
```
write_epoch.fetch_add(1, Acquire);   // odd → "writing"
let side = active.load(Relaxed);
buffers[side].lock().record_event(…);
write_epoch.fetch_add(1, Release);   // even → "idle"
```

The mutex on `buffers[side]` is only contended if the flush thread is
draining the *other* side at the exact moment the writer finishes and the
active index has just been swapped — which cannot happen because the flush
thread waits for the epoch to be even before draining.

**Flush thread (drain):**
```
for each handle:
    let old_side = handle.active.load(Relaxed);
    handle.active.store(1 - old_side, Release);  // swap
    // Wait for writer to finish any in-progress write on old_side
    while handle.write_epoch.load(Acquire) % 2 != 0 {
        spin / yield
    }
    // Now safe to drain old_side — writer is using new_side
    let buf = handle.buffers[old_side].lock();
    collector.accept_flush(buf.flush());
```

### Tradeoffs vs. current approach

| Aspect | Mutex + FlushEpoch | Left-Right |
|--------|-------------------|------------|
| Hot-path cost | `Mutex::lock()` (uncontended ~15–25 ns) | Two `fetch_add` (~10 ns total) |
| Flush-thread contention | Brief lock every 30 s on idle threads | None (atomic swap + spin-wait) |
| Memory per thread | 1 buffer + 1 `Arc<AtomicU64>` | 2 buffers + 2 atomics |
| Complexity | Low | Medium (epoch protocol, spin-wait) |
| Correctness risk | Low (standard mutex) | Medium (must get acquire/release right) |

### When to switch

Consider the Left-Right approach if:
- The `threadlocal_encode` benchmark shows >5% regression vs. the pre-mutex
  baseline (currently ~3% overhead end-to-end).
- Profiling shows `Mutex::lock` contention in the hot path (unlikely since
  the mutex is thread-local and almost never contended).
- The 30 s drain interval needs to be reduced to <1 s, increasing the
  frequency of flush-thread lock acquisition.
