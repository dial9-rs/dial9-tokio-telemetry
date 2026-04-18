# Analysis Pipeline

After parsing, run the analysis pipeline to derive higher-level structures. All functions are in `trace_analysis.js`.

## Standard pipeline

The `recipes` segment provides a ready-to-use `analyze(tracePath)` function that runs the full pipeline and returns `{ trace, workerIds, minTs, maxTs, spans, taskTimeline, schedDelays }`. Use it as-is or follow the steps below individually.

Pipeline steps:
1. Parse the trace: `parseTrace(buffer)` → `trace`
2. Extract worker IDs from non-queue, non-wake events
3. `buildWorkerSpans(events, workerIds, maxTs)` → reconstructs poll/park/active spans
4. `attachCpuSamples(cpuSamples, workerSpans)` → attaches profiling data to poll spans
5. `buildActiveTaskTimeline(taskSpawnTimes, taskTerminateTimes)` → task count over time
6. `computeSchedulingDelays(workerSpans, workerIds, wakesByTask)` → wake-to-poll latencies

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
