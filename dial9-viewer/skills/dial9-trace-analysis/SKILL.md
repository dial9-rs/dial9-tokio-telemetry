---
name: dial9-trace-analysis
description: Analysis pipeline API for dial9 traces. Covers analyzeTraces() aggregation, buildWorkerSpans, attachCpuSamples, scheduling delays, flamegraphs, span data, and the full return schema. Use when analyzing parsed traces or building custom analysis pipelines.
---

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

For directories with 1000+ files, `{ sample: 50 }` gives a quick overview. Follow up without `sample` for accurate percentiles.

## Pipeline steps

1. Parse: `for await (const trace of parseTrace(path))` yields one `ParsedTrace` per file
2. Extract worker IDs from non-queue, non-wake events
3. `buildWorkerSpans(events, workerIds, maxTs)` reconstructs poll/park/active spans
4. `attachCpuSamples(cpuSamples, workerSpans)` attaches profiling data to poll spans
5. `buildActiveTaskTimeline(taskSpawnTimes, taskTerminateTimes)` builds task count over time
6. `computeSchedulingDelays(workerSpans, workerIds, wakesByTask)` computes wake-to-poll latencies

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

Histogram objects are Node.js `perf_hooks.createHistogram()` instances. Key methods: `h.count`, `h.min`, `h.max`, `h.mean`, `h.percentile(p)` (p is 0..100).

## buildWorkerSpans(events, workerIds, maxTs)

Reconstructs structured spans from raw events. Returns:

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

- **Poll span**: PollStart to PollEnd. Duration is how long `.poll()` took.
- **Park span**: WorkerPark to WorkerUnpark. Worker had no work.
- **Active span**: WorkerUnpark to WorkerPark. `ratio` is CPU utilization (1.0 = fully on-CPU).

## attachCpuSamples(cpuSamples, workerSpans)

Attaches each CPU sample to the poll span it falls within. After calling:
- `poll.cpuSamples`: CPU profiling samples (source=0) during this poll
- `poll.schedSamples`: scheduling/off-CPU samples (source=1) during this poll
- `sample.spawnLoc`: spawn location of the task being polled

## buildActiveTaskTimeline(taskSpawnTimes, taskTerminateTimes)

Returns `{activeTaskSamples: [{t, count}]}`. Count at each point is tasks spawned but not yet terminated.

## computeSchedulingDelays(workerSpans, workerIds, wakesByTask)

Returns `[{wakeTime, pollTime, delay, taskId, wakerTaskId, worker, poll}]` sorted by wakeTime. Delay is `pollStart - wakeTime`.

## filterPointsOfInterest(filterType, workerSpans, workerIds, schedDelays, opts)

Filters for notable events. `filterType`: `"sched"`, `"long-poll"`, `"cpu-sampled"`, or `"wake-delay"`.

## buildFgData(samples, callframeSymbols)

Builds a flamegraph from CPU samples. Returns `{nodes, maxDepth, totalSamples}`.

## buildSpanData(customEvents)

Pairs SpanEnter/SpanExit custom events into span intervals per worker. Requires `Dial9TokioLayer`.

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
