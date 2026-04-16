# Loading and Parsing Traces

## ParsedTrace structure

`parseTrace(buffer, options?)` returns:

```
{
  events: TraceEvent[],          // PollStart, PollEnd, WorkerPark, WorkerUnpark, QueueSample, WakeEvent
  cpuSamples: CpuSample[],      // Periodic stack traces from perf/eBPF
  spawnLocations: Map<string, string>,    // spawn location ID → source location
  taskSpawnLocs: Map<number, string>,     // task ID → spawn location
  taskSpawnTimes: Map<number, number>,    // task ID → spawn timestamp (ns)
  taskTerminateTimes: Map<number, number>,// task ID → terminate timestamp (ns)
  callframeSymbols: Map<string, {symbol, location}>, // address → resolved symbol
  threadNames: Map<number, string>,       // thread ID → name
  clockOffsetNs: number|null,            // monotonic-to-wall-clock offset
  clockSyncAnchors: [{monotonicNs, realtimeNs}],
  runtimeWorkers: Map<string, number[]>, // runtime name → worker IDs
  truncated: boolean,
  timeFiltered: boolean,
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
const trace = await parseTrace(buffer, {
  maxEvents: 100000,        // Cap event count (metadata/symbols always parsed)
  startTime: 1000000000,    // Filter events to time range (absolute ns)
  endTime:   2000000000,
  onProgress: ({bytesRead, totalBytes, eventCount}) => { /* progress callback */ },
});
```

## Converting timestamps

Trace timestamps are monotonic nanoseconds. To convert to wall clock:

```javascript
if (trace.clockOffsetNs != null) {
  const wallNs = event.timestamp + trace.clockOffsetNs;
  const wallDate = new Date(Number(wallNs / 1_000_000n));  // if using BigInt
  // or: new Date(wallNs / 1e6)  // if already a JS number
}
```

To get relative time from trace start:
```javascript
const minTs = trace.events.reduce((m, e) => Math.min(m, e.timestamp), Infinity);
const relativeMs = (event.timestamp - minTs) / 1e6;
```

## Symbol resolution

CPU sample callchains contain raw addresses. Resolve them:

```javascript
const { formatFrame, symbolizeChain, deduplicateSamples } = require('./trace_parser.js');

// Resolve a full callchain to frame objects
const frames = symbolizeChain(sample.callchain, trace.callframeSymbols);
// [{symbol: "hyper::proto::h1::dispatch::Dispatcher<...>::poll_inner", location: "hyper-0.14.28/src/proto/h1/dispatch.rs:174"}, ...]

// Format a single frame for display (shortens generics, extracts filename)
const { text, docsUrl } = formatFrame(frames[0]);
// text: "Dispatcher::poll_inner dispatch.rs:174"
// docsUrl: "https://docs.rs/hyper/0.14.28/src/hyper/proto/h1/dispatch.rs.html#174"

// Deduplicate samples by stack trace
const groups = deduplicateSamples(trace.cpuSamples, trace.callframeSymbols);
// [{count: 8932, leaf: "__schedule", frames: [...]}, ...]
```

## Merging multiple trace files

Trace files can be concatenated back-to-back to form a single combined trace. Decompress any gzipped segments first, then concatenate the raw `.bin` files:

```bash
# Decompress and concatenate multiple segments
gunzip -k segment-001.bin.gz segment-002.bin.gz segment-003.bin.gz
cat segment-001.bin segment-002.bin segment-003.bin > combined.bin
```

Pass the combined file to `parseTrace` as usual. The parser handles multiple concatenated segments transparently — headers, string pools, and schemas are re-read at each segment boundary.

## Handling gzip

`parseTrace` automatically decompresses gzip input. You can pass `.bin.gz` files directly.
