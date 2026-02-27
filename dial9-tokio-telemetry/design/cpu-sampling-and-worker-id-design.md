# CPU Sampling & Worker ID Tracking — Current Design & Critique

## Overview

This document describes how CPU sampling (perf-based profiling and sched event capture) integrates with the tokio telemetry system, how worker IDs are tracked and mapped to OS thread IDs, and critically evaluates the current implementation.

---

## 1. Worker ID Resolution

### How it works

Worker IDs are resolved lazily per-thread via `resolve_worker_id()` in `recorder.rs`. The function:

1. Checks TLS cache (`WORKER_ID` cell). If already resolved, returns immediately.
2. Gets the current `std::thread::ThreadId`.
3. Iterates `RuntimeMetrics::worker_thread_id(i)` for `i in 0..num_workers()` to find a match.
4. Caches the result permanently in TLS (a thread's worker identity is stable).
5. On first resolution, eagerly registers the OS tid → worker mapping in `SharedState.worker_tids` (a `Mutex<HashMap<u32, usize>>`).

The `TID_EMITTED` TLS bool ensures the OS tid registration happens exactly once per thread.

### Where worker IDs appear

Every timestamped event carries a `worker_id: usize` (255 = `UNKNOWN_WORKER` sentinel for non-worker threads). Events that carry worker_id:
- `PollStart`, `PollEnd`
- `WorkerPark`, `WorkerUnpark`
- `CpuSample` (mapped from OS tid)
- `WakeEvent` carries `target_worker: u8` — the worker the waker was running on

### Wire format

Worker IDs are encoded as `u8` on the wire (format.rs), supporting up to 254 workers (255 = unknown). The in-memory representation uses `usize`.

---

## 2. CPU Profiling Integration

### Architecture

Two independent profiling modes, both feature-gated behind `cpu-profiling`:

```
Process-wide CPU profiler (CpuProfiler)
  - One perf_event_open fd, pid=0, cpu=-1
  - Samples ALL threads at configured Hz
  - Captures stack traces (frame-pointer based)
  - Stored in TelemetryRecorder (flush-thread owned)

Per-thread sched event profiler (SchedProfiler)  
  - One perf fd PER worker thread
  - Captures context switch events (period=1)
  - Stored in SharedState.sched_profiler (shared, Mutex-wrapped)
  - Workers call track/stop via on_thread_start/on_thread_stop hooks
```

### Data flow

```
Worker threads                    Flush thread (every 250ms)
─────────────                     ──────────────────────────
resolve_worker_id()               1. Sync worker_tids → CpuProfiler.tid_to_worker
  → registers OS tid              2. Sync worker_tids → SchedProfiler.tid_to_worker
    in SharedState.worker_tids    3. Drain CpuProfiler → Vec<TelemetryEvent>
                                  4. Drain SchedProfiler → Vec<TelemetryEvent>
                                  5. Optionally symbolicate (CallframeDef events)
                                  6. Write all CpuSample events to trace
```

### Timestamp correlation

Perf samples use `CLOCK_MONOTONIC` timestamps. The telemetry system uses `Instant::now()` (also monotonic). At profiler start, `clock_monotonic_ns()` captures the CLOCK_MONOTONIC value. The offset is stored in each profiler:

```
trace_relative_ns = perf_sample.time - clock_offset
```

This works because both clocks are monotonic and the offset is captured at the same moment as the trace `start_time`.

### tid → worker_id mapping

The mapping flows through two hops:

1. **Worker threads** register `(os_tid, worker_id)` in `SharedState.worker_tids` (once, on first `resolve_worker_id` call).
2. **Flush thread** copies this map into each profiler's local `tid_to_worker: HashMap<u32, usize>` before draining samples.

Samples from non-worker threads get `worker_id = 255` (UNKNOWN_WORKER).

### CpuSample event

```rust
CpuSample {
    timestamp_nanos: u64,     // trace-relative
    worker_id: usize,         // mapped from tid, 255 if unknown
    source: CpuSampleSource,  // CpuProfile (periodic) or SchedEvent (context switch)
    callchain: Vec<u64>,      // raw instruction pointer addresses
}
```

Wire format: `code(u8) + timestamp_us(u32) + worker_id(u8) + source(u8) + num_frames(u8) + frames(N * u64)`

### Inline symbolication (optional)

When `inline_callframe_symbols` is enabled, the flush thread:
1. For each address in a CpuSample's callchain, checks if it's been symbolicated.
2. If not, calls `perf_self_profile::resolve_symbol(addr)` to get symbol name + source location.
3. Emits a `CallframeDef { address, symbol, location }` event before the sample.
4. Tracks which addresses have been emitted per-file (cleared on rotation).

This is analogous to how `SpawnLocationDef` works for spawn locations.

---

## 3. SchedProfiler: Per-Thread Sched Events

The `SchedProfiler` captures context switches on each worker thread:

- Created during `TracedRuntimeBuilder::build()` if `with_sched_events()` was called.
- Stored in `SharedState.sched_profiler` (behind `Mutex<Option<...>>`).
- `on_thread_start` hook calls `profiler.track_current_thread()` → opens a perf fd for the calling thread.
- `on_thread_stop` hook calls `profiler.stop_tracking_current_thread()`.
- Flush thread drains samples, maps tids, writes `CpuSample` events with `source: SchedEvent`.

---

## 4. File Rotation Handling

Both `SpawnLocationDef` and `CallframeDef` are metadata events that must be re-emitted when the writer rotates to a new file. The system tracks this via:

- `FlushState.emitted_this_file: HashSet<SpawnLocationId>` — cleared on rotation.
- `callframe_emitted_this_file: HashSet<u64>` — cleared on rotation.
- `writer.take_rotated()` is checked before writing events and after `write_atomic` returns.

---

## 5. Critical Evaluation

### What works well

1. **TLS caching of worker IDs** — The permanent cache is correct (worker identity is stable) and makes the hot path essentially free after first resolution.

2. **Eager tid registration** — Registering the OS tid on first worker ID resolution means the CPU profiler can attribute samples immediately, without waiting for a flush cycle.

3. **Single process-wide sampler** — Avoids the complexity and overhead of per-thread perf fds for CPU profiling (sched events do need per-thread fds, but that's inherent to the event type).

4. **Clock offset approach** — Simple and correct for the trace durations involved.

5. **Separation of RawEvent vs TelemetryEvent** — Worker threads emit zero-allocation `RawEvent` with `&'static Location`; interning happens only on the flush thread.

### Issues and cleanup opportunities

#### A. Duplicated tid→worker sync logic

The flush path syncs `SharedState.worker_tids` into profilers in **two separate places**:

```rust
// Place 1: top of flush()
if let Some(ref mut profiler) = self.cpu_profiler {
    let tids = self.shared.worker_tids.lock().unwrap();
    for (&tid, &worker_id) in tids.iter() {
        profiler.register_worker(worker_id, tid);
    }
}

// Place 2: inside the sched_profiler drain block
{
    let mut shared_profiler = self.shared.sched_profiler.lock().unwrap();
    if let Some(ref mut profiler) = *shared_profiler {
        let tids = self.shared.worker_tids.lock().unwrap();
        for (&tid, &worker_id) in tids.iter() {
            profiler.register_worker(worker_id, tid);
        }
        cpu_events.extend(profiler.drain());
    }
}
```

This acquires `worker_tids` lock twice per flush. Should be consolidated: acquire once, sync both profilers.

#### B. `worker_tids` exists even without cpu-profiling feature

`SharedState.worker_tids: Mutex<HashMap<u32, usize>>` is always present, and `resolve_worker_id` always populates it. But it's only read by the CPU profiler. When `cpu-profiling` is disabled, this is dead code — a mutex and hashmap that get written to but never read.

**Suggestion:** Feature-gate the `worker_tids` field and the registration logic in `resolve_worker_id` behind `#[cfg(feature = "cpu-profiling")]`.

#### C. `sched_profiler` lives in SharedState behind Mutex, `cpu_profiler` lives in TelemetryRecorder

The two profilers have asymmetric ownership:
- `CpuProfiler` is owned by `TelemetryRecorder` (flush-thread only, no sharing needed).
- `SchedProfiler` is in `SharedState.sched_profiler` behind a `Mutex` because worker threads need to call `track_current_thread()` / `stop_tracking_current_thread()` from `on_thread_start/stop`.

This is correct but the asymmetry makes the flush path harder to follow. The `SchedProfiler` needs shared access because of the per-thread fd management, so this is somewhat inherent. But the drain path could be cleaner — right now it's a nested block with two separate lock acquisitions.

#### D. `current_worker_id()` doesn't pass SharedState for tid registration

```rust
pub(crate) fn current_worker_id(metrics: &ArcSwap<Option<RuntimeMetrics>>) -> u8 {
    match resolve_worker_id(metrics, None) {  // <-- None means no tid registration
        Some(id) if id <= 254 => id as u8,
        _ => 255,
    }
}
```

This is called from `SharedState::create_wake_event()` (via `Traced` waker). It passes `None` for `shared`, so the OS tid is never registered from this path. This is fine if the worker already resolved its ID through a poll/park event (which it almost certainly has), but it's a subtle implicit dependency.

#### E. The design doc mentions `WorkerThreadMap` event — it was never implemented (or was removed)

The original design doc (`cpu-profile-integration.md`) describes a `WorkerThreadMap` wire event (code 7) that would be emitted per-worker and written to the trace file. The progress doc marks Phase 1 as complete including "Add `WorkerThreadMap` to `RawEvent` and `TelemetryEvent`" and "Add wire code 7 for `WorkerThreadMap`".

But in the current code:
- There is no `WorkerThreadMap` variant in `RawEvent` or `TelemetryEvent`.
- Wire code 7 is `WIRE_WAKE_EVENT`, not `WorkerThreadMap`.
- The tid→worker mapping is purely in-memory (`SharedState.worker_tids`), never serialized.

The design evolved: instead of writing the mapping to the trace file, the mapping is used in-process to attribute `CpuSample` events to workers before writing. The samples already carry `worker_id` on the wire, so the reader doesn't need the raw tid mapping.

**The design docs are stale and should be updated** to reflect this. The progress doc is misleading.

#### F. `UNKNOWN_WORKER` is defined twice

```rust
// recorder.rs
const UNKNOWN_WORKER: usize = 255;

// cpu_profile.rs
const UNKNOWN_WORKER: usize = 255;
```

Should be a single shared constant.

#### G. One-shot log in flush path uses static AtomicBool

```rust
static LOGGED: std::sync::atomic::AtomicBool = std::sync::atomic::AtomicBool::new(false);
if !LOGGED.swap(true, Ordering::Relaxed) {
    eprintln!("[cpu-profiler] profiler active in flush path");
}
```

This is debug scaffolding that should probably be removed or converted to `tracing::debug!`.

#### H. `callframe_intern` grows unboundedly

The `callframe_intern: HashMap<u64, (String, Option<String>)>` in `TelemetryRecorder` accumulates every unique address seen across the entire trace. For long-running traces with diverse code paths, this could grow large. Not a practical problem at 99 Hz for ~71 minutes, but worth noting.

#### I. `resolve_worker_id` iterates all workers on every miss

When called from a non-worker thread, `resolve_worker_id` iterates all workers every time (since it never finds a match and never caches). This is O(num_workers) per call. For the common case (worker threads), the TLS cache makes this a non-issue. But if non-worker threads call this frequently, it could add up.

There's no negative caching — a non-worker thread will re-scan every time. Adding a `Cell<bool>` for "definitely not a worker" would fix this.

### Summary of recommended cleanups

| Priority | Item | Effort |
|----------|------|--------|
| Low | Consolidate duplicated tid→worker sync in flush() | Small |
| Low | Feature-gate `worker_tids` behind `cpu-profiling` | Small |
| Low | Deduplicate `UNKNOWN_WORKER` constant | Trivial |
| Low | Remove or downgrade the one-shot eprintln log | Trivial |
| Medium | Update stale design docs (WorkerThreadMap removal, wire code renumbering) | Medium |
| Low | Add negative caching for non-worker threads in `resolve_worker_id` | Small |

---

## Appendix: Thread Name Tracking (v13)

Added in wire format v13 to address the gap where all non-worker CPU samples were indistinguishable (all `worker_id=255`).

### What changed

- `CpuSample` now carries `tid: u32` on the wire (4 extra bytes per sample).
- New `ThreadNameDef { tid, name }` metadata event (wire code 10) maps OS tids to thread names.
- Flush thread reads `/proc/self/task/<tid>/comm` for non-worker tids, caches the result, and emits `ThreadNameDef` before the first sample from that tid in each file.
- `TraceReader` accumulates `thread_names: HashMap<u32, String>`.
- JS trace parser returns `threadNames` map alongside `cpuSamples`.

### Design rationale

- Follows the existing def-before-use pattern (`SpawnLocationDef`, `CallframeDef`).
- Thread name is read from procfs once per tid and cached — no per-sample overhead.
- Per-file emission tracking (`thread_name_emitted_this_file`) cleared on rotation, same as other defs.
- `tid` is always present on `CpuSample` (even for workers) so downstream tools can use it for any purpose without needing to know worker mappings.