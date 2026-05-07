---
name: dial9-trace-loading
description: Parse and load dial9 Tokio runtime trace files. Covers the ParsedTrace schema, event types, field definitions, parse options, time filtering, symbol resolution, and timestamp conversion. Use when loading traces or understanding the trace data model.
---

# Loading and Parsing Traces

## ParsedTrace structure

`parseTrace(path)` yields `ParsedTrace` objects:

```
{
  magic: string,                   // "D9TF" format identifier
  version: number,                 // trace format version
  events: TraceEvent[],          // PollStart, PollEnd, WorkerPark, WorkerUnpark, QueueSample, WakeEvent
  minTs: number|null,            // earliest event timestamp (ns), null if no events
  maxTs: number|null,            // latest event timestamp (ns), null if no events
  cpuSamples: CpuSample[],      // Periodic stack traces from perf/eBPF
  customEvents: CustomEvent[],   // SpanEnter/SpanExit events from tracing layer (requires dial9-tokio-telemetry tracing-layer feature)
  spawnLocations: Map<string, string>,    // spawn location ID → source location
  taskSpawnLocs: Map<number, string|null>,// task ID → spawn location (null if unknown)
  taskSpawnTimes: Map<number, number>,    // task ID → spawn timestamp (ns)
  taskTerminateTimes: Map<number, number>,// task ID → terminate timestamp (ns)
  callframeSymbols: Map<string, {symbol, location}|[{symbol, location}]>, // address → resolved symbol (array for inlined frames)
  threadNames: Map<number, string>,       // thread ID → name
  clockOffsetNs: number|null,            // monotonic-to-wall-clock offset
  clockSyncAnchors: [{monotonicNs, realtimeNs}],
  runtimeWorkers: Map<string, number[]>, // runtime name → worker IDs
  truncated: boolean,
  timeFiltered: boolean,
  filterStartTime: number|null,          // start of time range filter (ns), null if unfiltered
  filterEndTime: number|null,            // end of time range filter (ns), null if unfiltered
  hasCpuTime: boolean,                   // trace includes CPU time data
  hasSchedWait: boolean,                 // trace includes kernel scheduling wait data
  hasTaskTracking: boolean,              // trace includes task spawn/terminate events
  taskInstrumented: Map<number, boolean>, // task ID → whether task has tracing instrumentation
}
```

## Event types

```javascript
const EVENT_TYPES = {
  PollStart: 0,   // Worker begins polling a task
  PollEnd: 1,     // Worker finishes polling a task
  WorkerPark: 2,  // Worker goes to sleep (no work available)
  WorkerUnpark: 3,// Worker wakes up
  QueueSample: 4, // Global queue depth sample
  WakeEvent: 9,   // One task wakes another
};
```

## TraceEvent fields

| Field | Present on | Description |
|-------|-----------|-------------|
| `timestamp` | all | Monotonic nanoseconds |
| `workerId` | PollStart/End, Park/Unpark | Which worker thread |
| `taskId` | PollStart | Which task is being polled |
| `spawnLoc` | PollStart | Source location where the task was spawned |
| `localQueue` | PollStart, Park, Unpark | Worker's local queue depth |
| `globalQueue` | QueueSample | Global injection queue depth |
| `cpuTime` | Park, Unpark | Cumulative CPU time (ns) for this worker |
| `schedWait` | Unpark | Kernel scheduling wait time (ns) since last park |
| `wakerTaskId` | WakeEvent | Task that sent the wake |
| `wokenTaskId` | WakeEvent | Task that was woken |
| `targetWorker` | WakeEvent | Worker the wake was sent to |

## CpuSample fields

| Field | Description |
|-------|-------------|
| `timestamp` | Monotonic nanoseconds |
| `workerId` | Worker thread that was sampled |
| `tid` | OS thread ID |
| `source` | 0 = CPU profiling sample, 1 = scheduling (off-CPU) sample |
| `callchain` | Array of address strings like `"0x55cc6d053893"` |

## Parse options

```javascript
for await (const trace of parseTrace('/path/to/trace.bin', {
  maxEvents: 100000,        // Cap event count (metadata/symbols always parsed)
  startTime: 1000000000,    // Filter events to time range (absolute ns)
  endTime:   2000000000,
})) {
  // ...
}
```

## Converting timestamps

Trace timestamps are monotonic nanoseconds. To convert to wall clock:

```javascript
if (trace.clockOffsetNs != null) {
  const wallNs = event.timestamp + trace.clockOffsetNs;
  const wallDate = new Date(wallNs / 1e6);
}
```

To get relative time from trace start:

```javascript
const relativeMs = (event.timestamp - trace.minTs) / 1e6;
```

## Symbol resolution

```javascript
const { formatFrame, symbolizeChain, deduplicateSamples } = require('./trace_parser.js');

// Resolve a full callchain to frame objects
const frames = symbolizeChain(sample.callchain, trace.callframeSymbols);
// [{symbol: "hyper::proto::h1::dispatch::Dispatcher<...>::poll_inner", location: "hyper-0.14.28/src/proto/h1/dispatch.rs:174"}, ...]

// Format a single frame for display
const { text, docsUrl } = formatFrame(frames[0]);
// text: "Dispatcher::poll_inner dispatch.rs:174"

// Deduplicate samples by stack trace
const groups = deduplicateSamples(trace.cpuSamples, trace.callframeSymbols);
// [{count: 8932, leaf: "__schedule", frames: [...]}, ...]
```

## Handling gzip

`parseTrace` automatically decompresses gzip input. Pass `.bin.gz` files directly.

## Merging multiple trace files

Trace files can be concatenated back-to-back. The parser handles multiple concatenated segments transparently (headers, string pools, and schemas are re-read at each boundary).

```bash
gunzip -k segment-001.bin.gz segment-002.bin.gz segment-003.bin.gz
cat segment-001.bin segment-002.bin segment-003.bin > combined.bin
```
