# Analysis Pipeline

After parsing, run the analysis pipeline to derive higher-level structures. All functions are in `trace_analysis.js`.

## Standard pipeline

```javascript
const { parseTrace, EVENT_TYPES } = require('./trace_parser.js');
const { buildWorkerSpans, attachCpuSamples, buildActiveTaskTimeline,
        computeSchedulingDelays, filterPointsOfInterest, buildFgData } = require('./trace_analysis.js');

const trace = await parseTrace(fs.readFileSync('trace.bin'));

// 1. Extract worker IDs
const workerIds = [...new Set(
  trace.events.filter(e => e.eventType !== EVENT_TYPES.QueueSample && e.eventType !== EVENT_TYPES.WakeEvent)
    .map(e => e.workerId)
)].sort((a, b) => a - b);

const maxTs = trace.events.reduce((m, e) => Math.max(m, e.timestamp), -Infinity);

// 2. Build worker spans — reconstructs poll/park/active periods from raw events
const spans = buildWorkerSpans(trace.events, workerIds, maxTs);

// 3. Attach CPU samples to the poll spans they fall within
attachCpuSamples(trace.cpuSamples, spans.workerSpans);

// 4. Build active task count timeline
const taskTimeline = buildActiveTaskTimeline(trace.taskSpawnTimes, trace.taskTerminateTimes);

// 5. Compute scheduling delays (wake → poll latency)
const schedDelays = computeSchedulingDelays(spans.workerSpans, workerIds, spans.wakesByTask);
```

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

## filterPointsOfInterest(filterType, workerSpans, workerIds, schedDelays, hasSchedWait, opts)

Filters for notable events. `filterType` is one of:
- `"sched"` — Kernel scheduling delays >100µs on worker unpark
- `"long-poll"` — Polls longer than 1ms
- `"cpu-sampled"` — Polls that have CPU or scheduling samples attached
- `"wake-delay"` — Wake-to-poll delays >100µs

`opts.sortByWorst = true` sorts by severity instead of time.

Returns `[{time, worker, type, value, span, schedDelay?}]`.

## buildFgData(samples, callframeSymbols)

Builds a flamegraph from CPU samples. Returns `{nodes, maxDepth, totalSamples}` where each node has `{name, depth, x, w, count, self}`. `x` and `w` are fractions of total width (0–1).

Filter samples before passing to get per-spawn-location or per-worker flamegraphs:
```javascript
const workerSamples = trace.cpuSamples.filter(s => s.workerId === 0);
const fgData = buildFgData(workerSamples, trace.callframeSymbols);
```
