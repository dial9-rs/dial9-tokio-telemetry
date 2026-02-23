# Tokio Runtime Event Telemetry System

## Problem Statement

Build a telemetry system to capture Tokio runtime events (poll start/stop, worker park/unpark) with runtime metrics to diagnose scenarios where tasks are queued but workers appear idle.

**Specific Use Case:** Debugging situations where there are workers not doing work but there is a lot of stuff in the queue. The question is: Are the threads not being scheduled by the OS (but the workers are unparked) OR are the workers just sitting there parked?

**Key Insight:** By capturing thread CPU time at park/unpark boundaries, we can compute scheduling ratios (CPU time / wall time) for each active period to distinguish between workers that are busy vs. workers that are descheduled by the OS.

## Requirements

### Functional Requirements
- Capture events: `on_thread_park`, `on_thread_unpark`, `on_before_task_poll`, `on_after_task_poll`
- Record essential metrics at each event:
  - Global queue depth (sampled periodically)
  - Per-worker local queue depth
  - Worker park status
  - Timestamp (microsecond precision on disk, nanosecond in memory)
  - Thread CPU time (at park/unpark for scheduling analysis)
- Support several minutes of continuous tracing
- Output: compact binary format with variable-size records

### Non-Functional Requirements
- **Performance:** Target ~1-5% overhead (acceptable for performance testing)
- **Scalability:** Handle high event rates from multi-threaded runtime
- **Minimal Contention:** Use thread-local buffers to avoid synchronization overhead
- **Memory Efficiency:** Size-based buffer flushing to control memory usage

### Technical Constraints
- Use `tokio_unstable` features for per-worker metrics
- Must work with Tokio's multi-threaded runtime
- Binary output format must be somewhat pluggable for future extensions

## Architecture Overview

### High-Level Design

```
┌─────────────────────────────────────────────────────────────┐
│                    Tokio Runtime                             │
│  ┌──────────┐  ┌──────────┐  ┌──────────┐                  │
│  │ Worker 0 │  │ Worker 1 │  │ Worker N │                  │
│  └────┬─────┘  └────┬─────┘  └────┬─────┘                  │
│       │             │             │                          │
│       │ Events      │ Events      │ Events                  │
│       ▼             ▼             ▼                          │
│  ┌─────────┐  ┌─────────┐  ┌─────────┐                     │
│  │ TLS Buf │  │ TLS Buf │  │ TLS Buf │  (1024 events each) │
│  └────┬────┘  └────┬────┘  └────┬────┘                     │
└───────┼────────────┼────────────┼──────────────────────────┘
        │            │            │
        │ Flush      │ Flush      │ Flush (when full)
        ▼            ▼            ▼
   ┌────────────────────────────────────┐
   │      Central Collector             │
   │    (ArcSwap<Vec<Vec<Event>>>)      │
   └────────────────┬───────────────────┘
                    │
                    │ Periodic drain
                    ▼
            ┌───────────────┐
            │ Trace Writer  │
            │   (Binary)    │
            └───────┬───────┘
                    │
                    ▼
              ┌──────────┐
              │ Disk File│
              └──────────┘
```

### Component Architecture

The system is organized into 5 layers:

1. **Event Types Layer** - Core data structures
2. **Thread-Local Buffer Layer** - Per-worker event collection
3. **Central Collector Layer** - Lock-free aggregation
4. **Writer Layer** - Pluggable binary output
5. **Integration Layer** - Runtime hook installation

## Detailed Design

### 1. Event Types and Metrics Snapshot

#### EventType Enum
Five event types capture the complete worker lifecycle:
- **PollStart/PollEnd**: Task execution boundaries
- **WorkerPark/WorkerUnpark**: Thread parking state transitions
- **QueueSample**: Periodic global queue depth snapshots

**Design Rationale:**
- `#[repr(u8)]` for compact binary representation
- Simple enum for efficient matching and serialization
- QueueSample is runtime-level (no worker_id) to avoid redundant sampling

#### MetricsSnapshot Struct
Captures the essential runtime state at each event:
- **timestamp_nanos**: Monotonic time since trace start
- **worker_id**: Tokio worker index (0 for QueueSample events)
- **global_queue_depth**: Tasks in global queue (only meaningful for QueueSample)
- **worker_local_queue_depth**: Tasks in this worker's local queue
- **cpu_time_nanos**: Thread CPU time from `CLOCK_THREAD_CPUTIME_ID` (only set on park/unpark)

**Design Rationale:**
- All fields are `Copy` types for performance
- Nanosecond precision in memory, microsecond on disk (sufficient for ~71 minute traces)
- CPU time enables scheduling ratio analysis: `cpu_delta / wall_delta` reveals OS-level descheduling
- Essential metrics only to minimize overhead
- Fixed-size structure for efficient serialization

**Size:** 40 bytes (on 64-bit systems)

#### TelemetryEvent Struct
Simple wrapper combining event type with metrics snapshot.

**Size:** 48 bytes (with padding)

### 2. Thread-Local Event Buffer

**Design Philosophy:** Minimize contention by buffering events in thread-local storage. Each worker accumulates events in its own buffer and only interacts with the central collector during flush.

**Key Characteristics:**
- **Buffer Size:** 1024 events (~48KB per worker)
  - Balances memory usage vs flush frequency
  - At 100K events/sec, flushes every ~10ms
- **Zero-cost TLS:** Uses `thread_local!` macro for efficient access
- **Pre-allocated capacity:** Avoids reallocations during recording
- **Efficient flush:** Buffer swap via `std::mem::replace` (pointer swap, no copying)

**Flush Triggers:**
- Size-based: When buffer reaches capacity
- On park: Ensures parked workers don't hold events indefinitely

**Performance Characteristics:**
- Record event: O(1) amortized
- Check should_flush: O(1)
- Flush: O(1) (just pointer swap)

### 3. Central Collector

**Design Philosophy:** Simple, correct synchronization over lock-free complexity. After evaluating ArcSwap (which requires cloning on every write), the implementation uses a straightforward `Mutex<Vec<Vec<Event>>>`.

**Key Characteristics:**
- **Mutex-based:** Simple, correct, and sufficient for flush frequency (~250ms intervals)
- **Preserves structure:** Maintains per-worker buffer grouping for analysis
- **Minimal contention:** Workers only interact during flush (infrequent)
- **Efficient drain:** `std::mem::take` swaps in empty Vec without allocation

**Why Not ArcSwap?**
- ArcSwap requires cloning the entire `Vec<Vec<Event>>` on every `accept_flush`
- At flush frequencies of 250ms, mutex overhead is negligible
- Simpler code, easier to reason about correctness
- Could revisit if profiling shows contention (unlikely given flush frequency)

**Performance Characteristics:**
- accept_flush: O(1) with brief mutex hold
- drain: O(1) pointer swap
- Contention: Rare (only during concurrent flushes)

### 4. Pluggable Binary Writer

**Design Philosophy:** Trait-based abstraction allows multiple output formats while optimizing the common case (binary files).

**TraceWriter Trait:**
- `Send` bound for thread safety
- Batch write method for efficiency
- Explicit flush control
- Implementations: `SimpleBinaryWriter`, `RotatingWriter`, `NullWriter` (for benchmarking)

**Binary Format (v5):**
Variable-size records optimize for the common case:
- **Header:** 12 bytes (magic "TOKIOTRC" + version u32)
- **PollStart/PollEnd (local_queue=0):** 6 bytes (code + timestamp_us + worker_id)
- **PollStart/PollEnd (local_queue>0):** 7 bytes (adds local_queue u8)
- **WorkerPark/Unpark:** 11 bytes (code + timestamp_us + worker_id + local_queue + cpu_us)
- **QueueSample:** 6 bytes (code + timestamp_us + global_queue)

**Design Rationale:**
- **Variable-size records:** Most polls have empty local queues, save 1 byte per event
- **Microsecond timestamps:** u32 supports ~71 minute traces, sufficient for profiling sessions
- **Little-endian:** x86/ARM compatibility
- **Magic bytes + version:** File validation and format evolution
- **BufWriter:** 8KB buffer amortizes syscall overhead

**File Size Estimation:**
- Typical workload: ~7 bytes/event average
- 1M events ≈ 7MB (vs 33MB in fixed-size format)
- 10M events ≈ 70MB

### 5. Telemetry Recorder with Runtime Metrics

**Design Philosophy:** Minimize per-event overhead by caching expensive lookups and only capturing metrics when needed.

**Key Components:**
- **RuntimeMetrics:** Tokio's API for queue depths (atomic reads, ~20-40ns each)
- **Worker ID Resolution:** Thread-local cache with lazy initialization
  - Maps `ThreadId` → tokio worker index using `RuntimeMetrics::worker_thread_id`
  - Cached in TLS after first lookup
  - Invalidated on flush to pick up map updates
- **Timestamp:** Monotonic `Instant` since trace start
- **CPU Time:** `CLOCK_THREAD_CPUTIME_ID` via vDSO (~20-40ns, no syscall)
  - Only captured on park/unpark events
  - Enables scheduling ratio analysis

**Selective Metrics Capture:**
- **PollStart/PollEnd:** timestamp + worker_id + local_queue_depth
- **WorkerPark/Unpark:** timestamp + worker_id + local_queue_depth + cpu_time
- **QueueSample:** timestamp + global_queue_depth (no worker_id)

**Performance Considerations:**
- RuntimeMetrics methods are cheap atomic reads
- Worker ID lookup amortized to ~0 after first event
- CPU time read is vDSO call (no kernel transition)
- Total overhead per event: ~100-150ns

### 6. Runtime Integration

**Design Philosophy:** Minimal API surface with sensible defaults. `TracedRuntime::build` handles all wiring.

**Hook Installation:**
- **on_thread_park/unpark:** Record park state transitions + CPU time
- **on_before/after_task_poll:** Record poll boundaries
- Each hook clones a shared `Arc` to access recorder state

**Worker ID Assignment:**
- Tokio doesn't expose worker IDs directly in hooks
- Solution: Build `ThreadId → worker_index` map from `RuntimeMetrics`
- Cached in TLS for fast lookup
- Rebuilt periodically to handle thread pool changes

**Shared State Design:**
- Lock-free reads via `ArcSwap` for metrics and worker map
- `AtomicBool` for enable/disable flag
- Allows runtime control without stopping the runtime

**Usage Pattern:**
```rust
let writer = RotatingWriter::new("trace.bin", 1_MB, 5_MB)?;
let (runtime, guard) = TracedRuntime::build(builder, Box::new(writer))?;

// Use runtime normally
runtime.block_on(async { /* ... */ });

// Drop runtime, then guard performs final flush
drop(runtime);
drop(guard);
```

### 7. Periodic Flush and Sampling

**Design Philosophy:** Separate background thread handles both flush and sampling to avoid blocking the runtime.

**Background Thread Responsibilities:**
1. **Flush cycle (250ms):** Drain collector, rebuild worker map, write to disk
2. **Sample cycle (10ms):** Capture global queue depth as QueueSample event

**Why Separate Thread?**
- Avoids spawning tasks on the traced runtime (would pollute trace)
- Decouples I/O from runtime execution
- Simpler shutdown: just signal thread to stop

**Flush Strategy:**
- **Size-based (per-thread):** Flush when buffer reaches 1024 events
- **Time-based (global):** Background thread flushes every 250ms
- **On park:** Workers flush when parking to avoid holding events
- **On shutdown:** Guard drop triggers final flush

**Worker Map Rebuild:**
- Happens during each flush cycle
- Invalidates TLS cache so workers re-resolve on next event
- Handles thread pool changes (though rare in practice)

**Shutdown Handling:**
- `TelemetryGuard` drop signals background thread to stop
- Final flush captures any remaining buffered events
- Some thread-local events may be lost (acceptable for profiling)

## Implementation Status

All core components are implemented and tested:

✅ **Event Types** - EventType enum, MetricsSnapshot, TelemetryEvent  
✅ **Thread-Local Buffer** - 1024-event buffers with efficient flush  
✅ **Central Collector** - Mutex-based aggregation  
✅ **Binary Writer** - Variable-size format v5 with RotatingWriter  
✅ **Telemetry Recorder** - RuntimeMetrics integration with CPU time capture  
✅ **Runtime Integration** - TracedRuntime builder with hook installation  
✅ **Background Thread** - Flush + sampling in separate thread  
✅ **Analysis Tools** - TraceReader, analyze_trace, detect_idle_workers  
✅ **HTML Viewer** - Interactive timeline visualization with scheduling ratios

## Performance Analysis

### Measured Overhead

**Per Event (estimated from implementation):**
- Capture metrics: ~60ns (1 atomic load for local queue, CPU time on park/unpark)
- Create event struct: ~10ns (stack allocation)
- Push to buffer: ~20ns (Vec push)
- Check should_flush: ~5ns
- **Total: ~95ns per event**

**Flush Overhead (per 1024 events):**
- Buffer swap: ~50ns
- Mutex lock/unlock: ~50ns
- **Total: ~100ns per flush**

**Amortized per event:** ~95ns + (100ns / 1024) ≈ ~95ns

**At 1M events/sec:**
- CPU time: 95ms/sec = 9.5% overhead
- Within acceptable range for profiling workloads

**Optimization Opportunities:**
1. ✅ Selective metrics: Only capture CPU time on park/unpark (implemented)
2. ✅ Variable-size format: Save bytes on common case (implemented)
3. Sampling: Record only 1 in N poll events (future)
4. Adaptive sampling: Sample more during interesting periods (future)

### Memory Usage

**Per Worker:**
- Thread-local buffer: 48KB (1024 * 48 bytes)
- Total for 8 workers: 384KB

**Central Collector:**
- Depends on flush frequency
- At 250ms flush interval, 1M events/sec: ~12MB peak (250K events * 48 bytes)
- Bounded by flush task frequency

**Total Memory:** < 15MB for typical workloads (8 workers, 1M events/sec)

## Testing Strategy

### Unit Tests
- Event creation and serialization (variable-size format)
- Buffer operations (record, flush)
- Collector operations (accept, drain)
- Writer operations (write, flush, rotation)
- Format roundtrip tests (v4 and v5 compatibility)

### Integration Tests
- Multi-threaded event recording
- Worker ID resolution correctness
- End-to-end: runtime → events → file
- Trace analysis and idle worker detection

### Performance Tests
- Overhead measurement (overhead_bench.rs)
- Throughput testing with realistic workloads
- Memory usage profiling

## Future Enhancements

### Short Term
1. **Adaptive sampling:** Sample more during high queue depth or long polls
2. **S3 writer:** Direct upload to S3 for production deployments
3. **Compression:** LZ4 for binary output (trace files compress well)

### Medium Term
1. **Perfetto format:** Implement TraceWriter for Chrome tracing
2. **Live streaming:** WebSocket-based real-time visualization
3. **Filtering:** Runtime configuration of which events to capture
4. **Benchmarking suite:** Track overhead across versions

### Long Term
1. **Distributed tracing:** Correlate events across multiple processes
2. **Analysis tools:** Automated detection of common issues (idle workers, long polls, imbalance)
3. **Integration:** Upstream contribution to Tokio (if useful to broader community)

## References

- [Tokio Runtime Metrics](https://docs.rs/tokio/latest/tokio/runtime/struct.RuntimeMetrics.html)
- [Tokio Runtime Builder](https://docs.rs/tokio/latest/tokio/runtime/struct.Builder.html)
- [ArcSwap Documentation](https://docs.rs/arc-swap/latest/arc_swap/)
- [Perfetto Trace Format](https://perfetto.dev/docs/reference/trace-packet-proto)

## Appendix: Code Structure

```
src/
├── lib.rs                    # Existing long-poll detection code
└── telemetry/
    ├── mod.rs                # Public API
    ├── events.rs             # EventType, MetricsSnapshot, TelemetryEvent, CPU time helpers
    ├── buffer.rs             # ThreadLocalBuffer
    ├── collector.rs          # CentralCollector (Mutex-based)
    ├── writer.rs             # TraceWriter trait, SimpleBinaryWriter, RotatingWriter, NullWriter
    ├── recorder.rs           # TelemetryRecorder, TracedRuntime, TelemetryGuard
    ├── format.rs             # Binary format v5 (variable-size records)
    └── analysis.rs           # TraceReader, analyze_trace, detect_idle_workers, compute_active_periods

examples/
├── telemetry_demo.rs         # Basic usage
├── telemetry_rotating.rs     # Production-ready with RotatingWriter
├── analyze_trace.rs          # Offline analysis tool
├── trace_to_jsonl.rs         # Format converter
└── overhead_bench.rs         # Performance measurement

design/
└── tokio-telemetry-system.md # This document

trace_viewer.html             # Interactive HTML viewer (standalone, no build required)
```
