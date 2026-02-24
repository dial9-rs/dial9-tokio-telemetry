# CPU Profile Integration: Merging perf stack traces into tokio telemetry

## Goal

When a tokio worker thread has a long poll (visible in the telemetry trace as a wide PollStart→PollEnd span), we want to show **what the CPU was actually doing** during that time by attaching perf-sampled stack traces to the trace timeline.

## The Core Problem

We have two independent data sources:
1. **Tokio telemetry** — knows about workers, tasks, polls, parks (identified by `worker_id: usize`)
2. **perf-self-profile** — knows about CPU samples with stack traces (identified by `tid: u32`, the OS thread ID)

To merge them, we need to:
1. Map OS thread IDs (tids) to tokio worker IDs
2. Correlate perf sample timestamps with telemetry event timestamps
3. Attach the samples to the right place in the trace output

## Design

### 1. Thread ID ↔ Worker ID Mapping

The recorder already resolves worker IDs via `RuntimeMetrics::worker_thread_id()` which returns `std::thread::ThreadId`. But perf gives us raw OS `tid` (from `gettid()`). We need to bridge these.

**Approach:** At worker thread startup (first event from a worker), capture the OS tid via `gettid()` and emit a mapping event. The telemetry system already caches worker_id in TLS via `resolve_worker_id()` — we extend this to also capture the OS tid at the same time.

New event:
```
WorkerThreadMap { worker_id: usize, tid: u32 }
```

This is emitted once per worker thread when the worker ID is first resolved. The flush thread writes it as a metadata record (like SpawnLocationDef).

**Wire format:** `code(u8) + worker_id(u8) + tid(u32)` = 6 bytes, wire code 7.

### 2. Timestamp Correlation

Telemetry timestamps are `Instant::now().elapsed()` (monotonic clock, nanoseconds since trace start).

Perf sample timestamps are from the kernel's perf clock, which is `CLOCK_MONOTONIC` (nanoseconds since boot).

To correlate:
- At sampler start, capture both `Instant::now()` and `clock_gettime(CLOCK_MONOTONIC)`.
- `telemetry_ts = perf_ts - monotonic_at_start + instant_at_start.elapsed()`
- In practice: store `perf_clock_offset = clock_gettime(CLOCK_MONOTONIC) - 0` at the same moment we record `start_time = Instant::now()`. Then: `trace_ns = perf_sample.time - perf_clock_offset`.

Since both clocks are monotonic and we capture the offset at startup, drift is negligible for ~71 minute traces.

**Implementation:** The `PerfProfiler` (new integration struct) captures the offset at construction time. When converting samples, it applies the offset.

### 3. Integration Architecture

```
                    Tokio Runtime
    ┌──────────┐  ┌──────────┐  ┌──────────┐
    │ Worker 0 │  │ Worker 1 │  │ Worker N │
    │ tid=1234 │  │ tid=1235 │  │ tid=1236 │
    └────┬─────┘  └────┬─────┘  └────┬─────┘
         │             │             │
    TLS events    TLS events    TLS events
         │             │             │
         ▼             ▼             ▼
    ┌────────────────────────────────────┐
    │      Central Collector             │
    └────────────────┬───────────────────┘
                     │
                     ▼
              ┌──────────────┐
              │ Flush Thread │──── drains perf samples too
              └──────┬───────┘
                     │
                     ▼
              ┌──────────────┐
              │ Trace Writer │
              └──────────────┘

    ┌────────────────────────────────────┐
    │      PerfSampler (pid=0)           │
    │  samples all threads in process    │
    │  ring buffer accumulates samples   │
    └────────────────────────────────────┘
```

The `PerfSampler` runs with `pid=0` (current process), `cpu=-1` (all CPUs), so it captures samples from ALL threads including tokio workers. Each sample has a `tid` field identifying which thread was sampled.

**Key insight:** We don't need per-thread samplers. One process-wide sampler captures everything. The `tid` field in each sample lets us attribute it to the right worker.

### 4. New Event Type

```rust
CpuSample {
    timestamp_nanos: u64,  // converted to trace-relative time
    worker_id: u8,         // mapped from tid, or 255 if unknown
    callchain: Vec<u64>,   // instruction pointer addresses
}
```

**Wire format:**
```
code(u8) + timestamp_us(u32) + worker_id(u8) + num_frames(u8) + frames(num_frames * u64)
```
= 7 + 8*N bytes. Wire code 8.

We cap frames at 64 (u8 max 255, but stacks > 64 deep are unusual). Addresses are raw virtual addresses — symbolization happens offline in the viewer/analysis tool.

### 5. Integration Point: The Flush Thread

The existing flush thread (in `TelemetryGuard::build`) runs every 5ms and handles:
- Flushing telemetry events (every 250ms)
- Sampling global queue depth (every 10ms)

We add a third responsibility:
- **Draining perf samples (every flush cycle, ~250ms)**

During each flush cycle:
1. Call `sampler.drain_samples()`
2. For each sample, convert timestamp and map tid→worker_id
3. Write as `CpuSample` events interleaved with telemetry events

The tid→worker_id map is built from `WorkerThreadMap` events that were emitted by worker threads. The flush thread maintains this map.

### 6. API Surface

```rust
// New builder method
TracedRuntimeBuilder::with_cpu_profiling(self, config: CpuProfilingConfig) -> Self

pub struct CpuProfilingConfig {
    pub frequency_hz: u64,        // default: 99 (low overhead)
    pub event_source: EventSource, // default: SwCpuClock
    pub include_kernel: bool,      // default: false
}
```

When enabled:
- `PerfSampler::start()` is called during runtime build
- The sampler is moved into the flush thread
- Samples are drained and written during each flush cycle
- Sampler is stopped on guard drop

### 7. Symbolization Strategy

Raw addresses go into the trace file. Symbolization is deferred to analysis time:
- The analysis tool / HTML viewer resolves addresses using `/proc/self/exe` (or a saved copy)
- This avoids the overhead of symbolization during recording
- The `resolve_symbol()` function from perf-self-profile can be used at analysis time

For the wire format, we store raw `u64` addresses. The viewer can symbolize them client-side or we provide an offline symbolization pass that annotates the trace.

### 8. Wire Format Changes (v8 → v9)

New wire codes:
- **7: WorkerThreadMap** — `code(u8) + worker_id(u8) + tid(u32)` = 6 bytes
- **8: CpuSample** — `code(u8) + timestamp_us(u32) + worker_id(u8) + num_frames(u8) + frames[u64]` = 7 + 8N bytes

Both are backward-compatible additions (new codes, old readers skip unknown codes).

### 9. Viewer Integration

In the HTML trace viewer, CPU samples appear as marks on the worker timeline:
- During a long poll, you see sample markers with expandable stack traces
- Hovering shows the symbolized stack
- This directly answers "what was the CPU doing during this 50ms poll?"

## Implementation Plan

### Phase 1: Thread mapping infrastructure
- Add `gettid()` call to worker ID resolution in recorder
- Add `WorkerThreadMap` event type + wire format
- Emit mapping events from worker threads

### Phase 2: Perf sampler integration
- Add `perf-self-profile` as optional dependency (feature-gated)
- Create `CpuProfilingConfig` and builder API
- Integrate sampler lifecycle with `TelemetryGuard`
- Timestamp offset calculation

### Phase 3: Sample collection and writing
- Drain samples in flush thread
- Map tid→worker_id
- Write `CpuSample` events
- Update reader/analysis to handle new events

### Phase 4: Viewer
- Render CPU samples on timeline
- Offline symbolization tool
- Stack trace display in viewer

## Risks and Mitigations

| Risk | Mitigation |
|------|-----------|
| perf_event_open fails (containers, VMs) | Feature-gated, graceful fallback, SwCpuClock works almost everywhere |
| Timestamp drift between clocks | Both monotonic, offset captured atomically at start, negligible for <71min |
| High sample rate overhead | Default 99 Hz (not 999), configurable |
| Large trace files from callchains | Cap at 64 frames, variable-length encoding |
| Frame pointers not available | Document requirement, degrade gracefully (empty callchains) |
