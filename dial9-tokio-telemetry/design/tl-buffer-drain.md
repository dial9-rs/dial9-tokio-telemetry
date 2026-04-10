# Thread-Local Buffer Drain Design

## Problem

On lightly-loaded systems, thread-local (TL) buffers (1 MB default) take a
long time to fill. Since `RotatingWriter` rotates every ~60 s, events from a
given time window can span multiple trace files. Fully-silent threads (no
events flowing) never flush at all until thread exit.

## Current Approach: Mutex + FlushEpoch

Each `ThreadLocalBuffer` is wrapped in `Arc<Mutex<…>>`. On first use, a
`TlBufferHandle { Weak<Mutex<TLB>>, FlushEpoch }` is registered in
`SharedState`. Every ~30 s the flush thread calls `drain_all_tl_buffers()`:

1. Bumps a global `drain_epoch` counter.
2. Iterates all registered handles.
3. Reads each handle's `FlushEpoch` (relaxed). If it matches the current
   epoch, the owning thread self-flushed recently — **skip** (zero
   contention with busy workers).
4. Otherwise, upgrades the `Weak`, locks the buffer, and flushes pending
   events into the `CentralCollector`.
5. Prunes dead `Weak` handles via `retain`.

### Performance characteristics

- **Busy workers**: never locked by the flush thread. The `FlushEpoch` check
  is a single relaxed atomic load — no cache-line contention.
- **Idle workers**: locked briefly (~µs) every 30 s. The mutex is almost
  always uncontended because the owning thread is idle.
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
