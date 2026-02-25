# Wake Events + Task Dump Capture — Implementation Progress

## Overview
Porting `Traced<F>` future wrapper from bd1635b to the current (post-refactor) codebase.
Two new capabilities:
1. **Wake events** — who woke a task, from which worker
2. **Task dump capture** — backtrace when a future is pending longer than a threshold

Design docs: `progress/wake-events-design.md` and `progress/task-dump-capture-design.md`

## Status: 8/13 Tasks Complete

---

## Phase 1: Wake Events ✅

### Task 0: Progress doc ✅

### Task 1.1: Add WakeEvent to TelemetryEvent + RawEvent ✅
**Files**: `src/telemetry/events.rs`
- Added `TelemetryEvent::WakeEvent { timestamp_nanos, waker_task_id, woken_task_id, target_worker }`
- Added `RawEvent::WakeEvent` with same fields
- Updated `timestamp_nanos()` (returns Some), `worker_id()` (returns None), `is_runtime_event()` (true)

### Task 1.2: WakeEvent wire format ✅
**Files**: `src/telemetry/format.rs`
- Bumped VERSION to 9, added WIRE_WAKE_EVENT = 7
- Wire format: code(1) + timestamp_us(4) + waker_task_id(4) + woken_task_id(4) + target_worker(1) = 14 bytes
- Added write/read cases in `write_event`/`read_event`, updated `wire_event_size`
- Roundtrip test passes

### Task 1.3: SharedState pub(crate) + current_worker_id() ✅
**Files**: `src/telemetry/recorder.rs`
- Made `SharedState` and all fields `pub(crate)`
- Added `current_worker_id(metrics) -> u8` helper (255 for unknown)

### Task 1.4: Traced<F> wrapper + waker instrumentation ✅
**Files**: `src/traced.rs` (new), `src/lib.rs`
- `Traced<F>` wraps inner future, intercepts waker via custom `RawWakerVTable`
- `TracedWakerData` holds inner waker + woken_task_id + Arc<SharedState>
- `record_wake_event()` emits to TLS buffer (no locks on hot path)
- Uses `tokio::task::try_id()` for waker_task_id, `current_worker_id()` for target_worker

### Task 1.5: TelemetryHandle::traced_handle() + wire WakeEvent ✅
**Files**: `src/telemetry/recorder.rs`
- Added `TelemetryHandle::traced_handle() -> TracedHandle`
- WakeEvent wired through `write_raw_event` match

### Task 1.6: Update analysis + trace_to_jsonl ✅
**Files**: `src/telemetry/analysis.rs`
- `analyze_trace` skips WakeEvent (no analysis yet)
- `TraceReader::read_event` passes WakeEvent through as runtime event
- `trace_to_jsonl` works automatically via `Serialize` derive on `TelemetryEvent`

### Task 1.7: Build and test end-to-end ✅
- All 68 tests pass, all examples compile
- WakeEvent roundtrip test added to format.rs

---

## Phase 2: Task Dump Capture (NOT STARTED)

### Task 2.1: FrameDef + TaskPending variants + wire format
**Files**: `src/telemetry/events.rs`, `src/telemetry/format.rs`
- Add `TelemetryEvent::FrameDef { id: u16, frame: String }` (wire code 8, 5+N bytes)
- Add `TelemetryEvent::TaskPending { timestamp_nanos, task_id, duration_us, frame_ids }` (wire code 9, 14+2N bytes)
- Update read/write/size

### Task 2.2: PendingTrace collector + frame interning
**Files**: `src/telemetry/collector.rs`, `src/telemetry/recorder.rs`
- Add `PendingTrace` struct to collector with `Mutex<Vec<PendingTrace>>`
- Add frame interning to `FlushState` (parallel to spawn location interning)

### Task 2.3: Task dump capture in Traced<F>::poll()
**Files**: `src/traced.rs`
- Add `TracedConfig { pending_threshold: Duration }` (default 1ms)
- On Pending: `Trace::capture(|| inner.poll(cx))`, store trace + Instant
- On next poll: if duration > threshold, send PendingTrace to collector

### Task 2.4: Flush thread backtrace resolution
**Files**: `src/telemetry/recorder.rs`
- `write_pending_trace()`: resolve_backtraces → intern frames → write FrameDef + TaskPending
- Drain pending_traces in flush() after regular event batches

### Task 2.5: Build and test end-to-end

---

## Key Design Decisions

1. **No locks on wake hot path**: Waker emits WakeEvent via TLS buffer (same path as poll hooks)
2. **TelemetryEvent enum**: New events are just enum variants — no separate write methods needed on TraceWriter
3. **PendingTrace separate from RawEvent**: `Trace` is not `Copy`, goes through `Mutex<Vec<PendingTrace>>` in collector
4. **Frame interning in FlushState**: Same pattern as spawn location interning, flush-thread-local
5. **Wire format v9**: WakeEvent=7, FrameDef=8, TaskPending=9
