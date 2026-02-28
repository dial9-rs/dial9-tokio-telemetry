# CPU Sampling & Worker ID Tracking — Current Design

## Overview

This document describes how CPU sampling (perf-based profiling and sched event capture) integrates with the tokio telemetry system, and how worker IDs are tracked and mapped to OS thread IDs.

---

## 1. Worker ID Resolution

### How it works

Worker IDs are resolved lazily per-thread via `resolve_worker_id()` in `recorder.rs`. The function:

1. Checks TLS cache (`WORKER_ID` cell). If already resolved, returns immediately.
2. Checks the `NOT_A_WORKER` negative cache. If set, returns `None` immediately (avoids re-scanning on every call from non-worker threads).
3. Gets the current `std::thread::ThreadId`.
4. Iterates `RuntimeMetrics::worker_thread_id(i)` for `i in 0..num_workers()` to find a match.
5. On a match, caches the result permanently in TLS (a thread's worker identity is stable).
6. On first resolution, eagerly registers the OS tid → worker mapping in `SharedState.worker_tids` (a `#[cfg(feature = "cpu-profiling")] Mutex<HashMap<u32, usize>>`).
7. If metrics are available but no match is found, sets `NOT_A_WORKER` to avoid future scans.

The `TID_EMITTED` TLS bool ensures the OS tid registration happens exactly once per thread.

### Where worker IDs appear

Every timestamped event carries a `worker_id: usize` (255 = `UNKNOWN_WORKER` sentinel for non-worker threads). Events that carry worker_id:
- `PollStart`, `PollEnd`
- `WorkerPark`, `WorkerUnpark`
- `CpuSample` (mapped from OS tid)
- `WakeEvent` carries `target_worker: u8` — the worker the waker was running on

### Wire format

Worker IDs are encoded as `u8` on the wire (`format.rs`), supporting up to 254 workers (255 = unknown). The in-memory representation uses `usize`.

### `UNKNOWN_WORKER` constant

`UNKNOWN_WORKER: usize = 255` is defined as a `pub const` in `events.rs` and imported by both `recorder.rs` and `cpu_profile.rs`.

---

## 2. CPU Profiling Integration

### Architecture

Two independent profiling modes, both feature-gated behind `cpu-profiling`:

```
Process-wide CPU profiler (CpuProfiler)
  - One perf_event_open fd, pid=0, cpu=-1
  - Samples ALL threads at configured Hz
  - Captures stack traces (frame-pointer based)
  - Owned by TelemetryRecorder (flush-thread only)

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
resolve_worker_id()               1. Acquire worker_tids lock once:
  → registers OS tid                   sync into CpuProfiler.tid_to_worker
    in SharedState.worker_tids         sync into SchedProfiler.tid_to_worker
                                  2. Drain CpuProfiler → Vec<TelemetryEvent>
                                       (eagerly caches thread names during drain)
                                  3. Drain SchedProfiler → Vec<TelemetryEvent>
                                  4. For each CpuSample:
                                       emit ThreadNameDef if new non-worker tid
                                       optionally emit CallframeDef (symbolication)
                                  5. Write all CpuSample events to trace
```

The tid→worker sync acquires `worker_tids` exactly once per flush cycle and updates both profilers before releasing the lock.

### Timestamp correlation

Perf samples use `CLOCK_MONOTONIC` timestamps. The telemetry system uses `Instant::now()` (also monotonic). At profiler start, `clock_monotonic_ns()` captures the `CLOCK_MONOTONIC` value and stores it as `clock_offset` in each profiler:

```
trace_relative_ns = perf_sample.time - clock_offset
```

This works because both clocks are monotonic and the offset is captured at the same moment as the trace `start_time`.

### tid → worker_id mapping

The mapping flows through two hops:

1. **Worker threads** register `(os_tid, worker_id)` in `SharedState.worker_tids` (once, on first `resolve_worker_id` call). The field is `#[cfg(feature = "cpu-profiling")]` and the registration logic is likewise gated, so there is no overhead when the feature is disabled.
2. **Flush thread** copies this map into each profiler's local `tid_to_worker: HashMap<u32, usize>` before draining samples.

Samples from non-worker threads get `worker_id = 255` (UNKNOWN_WORKER).

### CpuSample event

```rust
CpuSample {
    timestamp_nanos: u64,     // trace-relative
    worker_id: usize,         // mapped from tid, 255 if unknown
    tid: u32,                 // OS thread ID
    source: CpuSampleSource,  // CpuProfile (periodic) or SchedEvent (context switch)
    callchain: Vec<u64>,      // raw instruction pointer addresses
}
```

Wire format: `code(u8) + timestamp_us(u32) + worker_id(u8) + tid(u32) + source(u8) + num_frames(u8) + frames(N * u64)`

Fixed overhead: 12 bytes per sample (before frames).

### Inline symbolication (optional)

When `inline_callframe_symbols` is enabled, the flush thread:
1. For each address in a CpuSample's callchain, checks if it's been emitted in the current file.
2. If not, and not yet interned, calls `perf_self_profile::resolve_symbol(addr)` to get symbol name + source location, and caches in `callframe_intern`.
3. Emits a `CallframeDef { address, symbol, location }` event before the sample.
4. Tracks which addresses have been emitted per-file in `callframe_emitted_this_file` (cleared on rotation).

This is analogous to how `SpawnLocationDef` works for spawn locations. `callframe_intern` is retained across file rotations to avoid redundant symbolication work; only the per-file emission tracking is cleared.

---

## 3. SchedProfiler: Per-Thread Sched Events

The `SchedProfiler` captures context switches on each worker thread:

- Created during `TracedRuntimeBuilder::build()` if `with_sched_events()` was called.
- Stored in `SharedState.sched_profiler` (behind `Mutex<Option<...>>`).
- `on_thread_start` hook calls `profiler.track_current_thread()` → opens a perf fd for the calling thread.
- `on_thread_stop` hook calls `profiler.stop_tracking_current_thread()`.
- Flush thread drains samples, maps tids, writes `CpuSample` events with `source: SchedEvent`.

The asymmetric ownership between `CpuProfiler` (flush-thread-only, owned by `TelemetryRecorder`) and `SchedProfiler` (shared via `Mutex` in `SharedState`) is intentional: the sched profiler must be accessible from worker thread callbacks to manage per-thread fds.

---

## 4. Thread Name Tracking

All non-worker CPU samples carry `worker_id = 255`, which makes them indistinguishable without additional context. Thread names resolve this.

- `CpuSample` carries `tid: u32` on the wire for all samples (worker and non-worker alike).
- New `ThreadNameDef { tid, name }` metadata event (wire code 10) maps OS tids to thread names.
- During drain, `CpuProfiler` eagerly reads `/proc/self/task/<tid>/comm` for non-worker tids and caches the result in `tid_to_name`. This is done at drain time while the thread is still alive, before it might exit and the procfs entry disappear.
- The flush thread pulls names from `CpuProfiler.tid_to_name` into `TelemetryRecorder.thread_name_intern` and emits `ThreadNameDef` before the first sample from each tid in each file.
- Per-file emission tracking (`thread_name_emitted_this_file`) is cleared on rotation, same as other defs.
- `tid` is always present on `CpuSample` (even for workers) so downstream tools can use it for any purpose without needing to decode worker mappings.

---

## 5. File Rotation Handling

`SpawnLocationDef`, `CallframeDef`, and `ThreadNameDef` are metadata events that must be re-emitted when the writer rotates to a new file. The system tracks this via:

- `FlushState.emitted_this_file: HashSet<SpawnLocationId>` — cleared on rotation.
- `callframe_emitted_this_file: HashSet<u64>` — cleared on rotation.
- `thread_name_emitted_this_file: HashSet<u32>` — cleared on rotation.
- `writer.take_rotated()` is checked before writing events and after `write_atomic` returns.

---

## 6. Wire Codes (v13)

| Code | Event |
|------|-------|
| 0 | `PollStart` |
| 1 | `PollEnd` |
| 2 | `WorkerPark` |
| 3 | `WorkerUnpark` |
| 4 | `QueueSample` |
| 5 | `SpawnLocationDef` |
| 6 | `TaskSpawn` |
| 7 | `WakeEvent` |
| 8 | `CpuSample` |
| 9 | `CallframeDef` |
| 10 | `ThreadNameDef` |

The tid→worker mapping is resolved in-process before writing; there is no `WorkerThreadMap` wire event. `CpuSample` events carry an already-resolved `worker_id` on the wire, so readers do not need access to the raw tid mapping.