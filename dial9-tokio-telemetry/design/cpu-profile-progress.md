# CPU Profile Integration — Progress

## Status: Phase 1-3 complete

### Phase 1: Thread mapping infrastructure ✅
- [x] Design doc written (`design/cpu-profile-integration.md`)
- [x] Add `gettid()` helper (`current_tid()` in events.rs)
- [x] Add `WorkerThreadMap` to `RawEvent` and `TelemetryEvent`
- [x] Add wire code 7 for `WorkerThreadMap` in format.rs (read + write)
- [x] Emit mapping from recorder when worker ID first resolved (TID_EMITTED TLS)
- [x] Update `TraceReader` to accumulate `worker_tids` map

### Phase 2: Perf sampler integration ✅
- [x] Add `perf-self-profile` as optional dep with `cpu-profiling` feature gate
- [x] Create `CpuProfilingConfig` struct
- [x] Add `with_cpu_profiling()` to `TracedRuntimeBuilder`
- [x] Timestamp offset calculation (perf CLOCK_MONOTONIC → trace-relative via `clock_monotonic_ns()`)
- [x] Sampler lifecycle: started in `build()`, stored in `TelemetryRecorder`, stopped on drop
- [x] Graceful fallback if perf_event_open fails (warning printed, profiling disabled)

### Phase 3: Sample collection and writing ✅
- [x] Add `CpuSample` event type + wire code 8 (variable length: 7 + 8*N bytes)
- [x] `CpuProfiler::drain()` converts perf samples → `CpuSample` events with tid→worker mapping
- [x] Flush thread drains profiler during each flush cycle
- [x] `WorkerThreadMap` events update profiler's tid map during flush
- [x] `TraceReader` handles new event types (CpuSample returned as runtime event, WorkerThreadMap accumulated)
- [x] All 71 tests pass with and without `cpu-profiling` feature

### Phase 4: Viewer (not started)
- [ ] Render CPU samples on timeline
- [ ] Offline symbolization tool

## Files changed
- `src/telemetry/events.rs` — `WorkerThreadMap` + `CpuSample` variants, `current_tid()` helper
- `src/telemetry/format.rs` — v9 wire format, codes 7+8, read/write
- `src/telemetry/recorder.rs` — `resolve_worker_id` emits tid map, profiler lifecycle in flush
- `src/telemetry/analysis.rs` — `TraceReader.worker_tids`, handles new events
- `src/telemetry/cpu_profile.rs` — NEW: `CpuProfiler`, `CpuProfilingConfig`, clock offset
- `src/telemetry/mod.rs` — module + export
- `Cargo.toml` — optional `perf-self-profile` dep, `cpu-profiling` feature
- `design/cpu-profile-integration.md` — NEW: full design doc
