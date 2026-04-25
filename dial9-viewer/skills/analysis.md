# Analysis Pipeline

After parsing, run the analysis pipeline to derive higher-level structures. All functions are in `trace_analysis.js`.

## Quick reference

For aggregated results across all files (recommended):

```javascript
const { analyzeTraces } = require('./analyze.js');
const result = await analyzeTraces('/path/to/traces/'); // options: { sample, force }
// result.longPolls, result.workerSpans, result.schedDelayHist, result.cpuGroups, result.spanStats
```

For per-trace raw data (flamegraphs, field filtering, wake chains):

```javascript
const { parseTrace } = require('./trace_parser.js');
const { buildWorkerSpans, attachCpuSamples } = require('./trace_analysis.js');

for await (const trace of parseTrace('/path/to/traces/')) {
  // full ParsedTrace with events, cpuSamples, callframeSymbols, etc.
}
```

For directories with 1000+ files, `{ sample: 50 }` gives a quick initial overview (a few seconds). Follow up without `sample` for accurate percentiles and tail latency.

For progress on large directories, pass `onParseProgress` and `onAnalysisProgress` callbacks:

```javascript
const result = await analyzeTraces('/path/to/traces/', {
  onParseProgress: ({ done, total, cached }) => process.stderr.write(`\rparsing: [${done}/${total}]${cached ? ` (${cached} cached)` : ''}`),
  onParseComplete: () => process.stderr.write('\n'),
  onAnalysisProgress: ({ done, total }) => process.stderr.write(`\ranalyzing: [${done}/${total}]`),
});
process.stderr.write('\n');
```

## Standard pipeline

Use `analyzeTraces(path)` from `analyze.js` to run the full pipeline over a single file or directory. It returns an aggregated result object (see [analyzeTraces return schema](#analyzetraces-return-schema) below). Use it as-is or follow the steps below individually.

Pipeline steps:
1. Parse the trace: `for await (const trace of parseTrace(path))` yields one `ParsedTrace` per file
2. Extract worker IDs from non-queue, non-wake events
3. `buildWorkerSpans(events, workerIds, maxTs)` → reconstructs poll/park/active spans
4. `attachCpuSamples(cpuSamples, workerSpans)` → attaches profiling data to poll spans
5. `buildActiveTaskTimeline(taskSpawnTimes, taskTerminateTimes)` → task count over time
6. `computeSchedulingDelays(workerSpans, workerIds, wakesByTask)` → wake-to-poll latencies

For directories, `parseTrace` yields one `ParsedTrace` per file. See the `recipes` segment for the boilerplate.

## analyzeTraces return schema

`analyzeTraces(path, opts?)` returns a single object aggregated across all trace files:

```
{
  // ── Metadata ──
  workerIds: number[],              // sorted worker thread IDs
  minTs: number,                    // earliest timestamp (ns)
  maxTs: number,                    // latest timestamp (ns)
  durationMs: number,               // (maxTs - minTs) in milliseconds
  eventCount: number,               // total events processed
  cpuSampleCount: number,           // total CPU profiling samples
  onCpuSampleCount: number,         // samples where thread was on-CPU (source=0)
  offCpuSampleCount: number,        // samples where thread was off-CPU/descheduled (source=1)
  taskSpawnCount: number,           // total tasks spawned
  taskAliveAtEnd: number,           // tasks spawned but not terminated by trace end
  maxLocalQueue: number,            // peak local work-stealing queue depth

  // ── Per-worker summaries ──
  workerSpans: {
    [workerId]: {
      utilization: number,          // fraction of time active (0..1)
      avgCpuRatio: number,          // average CPU ratio during active spans
      pollCount: number,
      parkCount: number,
      activeCount: number,
      schedWaits: number[],         // kernel scheduling delays (ns), sorted descending
    }
  },

  // ── Scheduling delays ──
  schedDelayStats: {
    total: number,                  // total scheduling delay events
    highCount: number,              // delays > 1ms
    worst: [{wakeTime, pollTime, delay, taskId, wakerTaskId, worker, poll}],  // top 100 by delay
  },
  schedDelays: [{wakeTime, pollTime, delay, taskId, wakerTaskId, worker, poll}],  // same as schedDelayStats.worst
  schedDelayHist: Histogram|null,    // Node.js perf_hooks Histogram of all delay values (ns), null if no delays

  // ── Long polls ──
  longPolls: [{dur, poll, worker}], // polls > 1ms, top 100 sorted by duration descending
                                    // poll: {start, end, taskId, spawnLoc}

  // ── Queue depth ──
  queueDepthStats: {
    max: number,                    // peak global queue depth
    avg: number,                    // average global queue depth
    samples: number,                // number of queue depth samples
  },

  // ── Task lifecycle ──
  taskTimeline: {
    activeTaskSamples: [{t, count}],  // task count over time, sorted by t
  },
  taskSpawnLocs: Map<taskId, string|null>,  // taskId → spawn location string (null if unknown)
  taskSpawnTimes: Map<taskId, number>,      // taskId → spawn timestamp (ns)
  taskTerminateTimes: Map<taskId, number>,  // taskId → termination timestamp (ns)

  // ── CPU profiling ──
  callframeSymbols: Map<address, {symbol, location}|[{symbol, location}]>, // address → resolved symbol (array for inlined frames)
  cpuGroups: [{count, leaf, leafRaw, frames}],       // on-CPU sample groups, sorted by count descending
  schedGroups: [{count, leaf, leafRaw, frames}],     // off-CPU sample groups, sorted by count descending

  // ── Histograms ──
  spanStats: Map<spanName, Histogram>,      // tracing span duration histograms (ns)
  pollDurationByLoc: Map<spawnLoc, Histogram>,  // poll duration histograms by spawn location (ns)
}
```

Histogram objects are Node.js `perf_hooks.createHistogram()` instances. Key methods: `h.count`, `h.min`, `h.max`, `h.mean`, `h.percentile(p)` (where p is 0..100).

## buildWorkerSpans(events, workerIds, maxTs)

Reconstructs structured spans from raw events using a state machine.

Returns:
```
{
  workerSpans: {
    [workerId]: {
      polls: [{start, end, taskId, spawnLoc, cpuSamples?, schedSamples?}],
      parks: [{start, end, schedWait}],
      actives: [{start, end, ratio}],  // ratio = CPU time / wall time
      cpuSampleTimes: number[],
    }
  },
  queueSamples: [{t, global}],
  workerQueueSamples: {[workerId]: [{t, local}]},
  maxLocalQueue: number,
  wakesByTask: {[taskId]: [{timestamp, wakerTaskId, targetWorker}]},
  wakesByWorker: {[workerId]: [{timestamp, wakerTaskId, wokenTaskId}]},
}
```

Key concepts:
- **Poll span**: PollStart → PollEnd. Duration is how long a single `.poll()` call took.
- **Park span**: WorkerPark → WorkerUnpark. Worker had no work and went to sleep.
- **Active span**: WorkerUnpark → WorkerPark. Worker was awake and processing tasks. `ratio` is CPU utilization (1.0 = fully on-CPU, <1.0 = some time descheduled by kernel).
- **schedWait**: On Unpark events, how long the kernel took to reschedule the worker thread after it was woken.

## attachCpuSamples(cpuSamples, workerSpans)

Attaches each CPU sample to the poll span it falls within (binary search). After calling:
- `poll.cpuSamples` — array of CPU profiling samples (source=0) during this poll
- `poll.schedSamples` — array of scheduling/off-CPU samples (source=1) during this poll
- `sample.spawnLoc` — set to the spawn location of the task being polled

## buildActiveTaskTimeline(taskSpawnTimes, taskTerminateTimes)

Returns `{activeTaskSamples: [{t, count}], taskFirstPoll}`. The count at each point is the number of tasks that have been spawned but not yet terminated. Useful for detecting task leaks.

## computeSchedulingDelays(workerSpans, workerIds, wakesByTask)

For each poll, finds the most recent wake event for that task before the poll started. The delay is `pollStart - wakeTime`. Returns:
```
[{wakeTime, pollTime, delay, taskId, wakerTaskId, worker, poll}]
```
Sorted by wakeTime. Large delays mean a task was woken but had to wait before being polled (workers were busy).

## filterPointsOfInterest(filterType, workerSpans, workerIds, schedDelays, opts)

Filters for notable events. `filterType` is one of:
- `"sched"` — Kernel scheduling delays >100µs on worker unpark
- `"long-poll"` — Polls longer than 1ms
- `"cpu-sampled"` — Polls that have CPU or scheduling samples attached
- `"wake-delay"` — Wake-to-poll delays >100µs

`opts`:
- `hasSchedWait: true` — enables the `"sched"` filter (requires schedWait data in trace)
- `sortByWorst: true` — sorts by severity instead of time

Returns `[{time, worker, type, value, span, schedDelay?}]`.

## buildFgData(samples, callframeSymbols)

Builds a flamegraph from CPU samples. Returns `{nodes, maxDepth, totalSamples}` where each node has `{name, depth, x, w, count, self}`. `x` and `w` are fractions of total width (0–1).

Filter samples before passing to get per-spawn-location or per-worker flamegraphs:
```javascript
const workerSamples = trace.cpuSamples.filter(s => s.workerId === 0);
const fgData = buildFgData(workerSamples, trace.callframeSymbols);
```

## buildSpanData(customEvents)

Pairs `SpanEnter`/`SpanExit` custom events into span intervals per worker. Requires the `tracing-layer` feature on `dial9-tokio-telemetry` and `Dial9TokioLayer` in the subscriber.

```javascript
const { spansByWorker, spanMeta, maxDepth } = buildSpanData(trace.customEvents);
```

Returns:
```
{
  spansByWorker: {
    [workerId]: [{start, end, spanId, spanName, fields, parentSpanId, depth}]
  },
  spanMeta: Map<spanId, {spanName, fields, parentSpanId}>,
  maxDepth: number,
}
```

Key concepts:
- **Span interval**: One enter/exit pair. A span re-entered across multiple polls produces multiple intervals with the same `spanId`.
- **fields**: User-defined span fields (e.g., `{request_id: "abc", metric_name: "cpu"}`). Base fields (`worker_id`, `span_id`, `span_name`) are excluded.
- **parentSpanId**: Only set for explicit parents (`span!(parent: &x, ..)`). Most `#[instrument]` spans have `null`. Use timestamp containment to infer nesting.
- **depth**: Computed from the parent chain. 0 for root spans, incremented for each ancestor.
- Schema names follow the pattern `SpanEnter:{target}::{name}:{file}:{line}` (one schema per callsite).
