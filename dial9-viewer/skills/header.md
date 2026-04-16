# dial9 Trace Analysis Skill

dial9 traces capture the internal behavior of a Tokio async runtime: task polling, worker thread activity, queue depths, CPU profiling samples, scheduling delays, and task lifecycle events. You can analyze them programmatically using Node.js.

## What traces capture

- **Poll events**: Every time a worker thread polls a task future (start/end timestamps, task ID, spawn location)
- **Worker lifecycle**: Park/unpark events with CPU time and kernel scheduling wait
- **Queue depth**: Periodic samples of the global injection queue
- **Task lifecycle**: Spawn and terminate events with spawn location
- **Wake events**: Which task woke which other task, and on which worker
- **CPU samples**: Periodic stack traces from perf/eBPF, attached to the poll they occurred in
- **Scheduling samples**: Stack traces captured when the kernel deschedules a worker thread (shows blocking calls)
- **Clock sync**: Monotonic-to-wall-clock anchors for correlating with external logs

## Quick start

Get the analysis toolkit:

```bash
dial9-viewer agents toolkit --path /tmp/d9-toolkit
node /tmp/d9-toolkit/analyze.js <trace.bin>
```

This copies `decode.js`, `trace_parser.js`, `trace_analysis.js`, and `analyze.js` into the target directory. Run `analyze.js` for a full diagnostic report, then edit any of the files to drill deeper.

### Parsing traces manually

```javascript
const fs = require('fs');
const { parseTrace, EVENT_TYPES } = require('./trace_parser.js');
const { buildWorkerSpans, attachCpuSamples, buildActiveTaskTimeline,
        computeSchedulingDelays, filterPointsOfInterest, buildFgData } = require('./trace_analysis.js');

const buf = fs.readFileSync('trace.bin');
const trace = await parseTrace(buf);

// Get worker IDs
const workerIds = [...new Set(
  trace.events.filter(e => e.eventType !== EVENT_TYPES.QueueSample && e.eventType !== EVENT_TYPES.WakeEvent)
    .map(e => e.workerId)
)].sort((a, b) => a - b);

const minTs = trace.events.reduce((m, e) => Math.min(m, e.timestamp), Infinity);
const maxTs = trace.events.reduce((m, e) => Math.max(m, e.timestamp), -Infinity);

// Build the full analysis pipeline
const spans = buildWorkerSpans(trace.events, workerIds, maxTs);
attachCpuSamples(trace.cpuSamples, spans.workerSpans);
const taskTimeline = buildActiveTaskTimeline(trace.taskSpawnTimes, trace.taskTerminateTimes);
const schedDelays = computeSchedulingDelays(spans.workerSpans, workerIds, spans.wakesByTask);
```

## Fetching traces from S3

If `dial9-viewer` is running (e.g. on port 3000), fetch traces via its API:

```javascript
// Search for traces
const resp = await fetch('http://localhost:3000/api/search?bucket=BUCKET&q=2026-04-09/19');
const objects = await resp.json(); // [{key, size, last_modified}, ...]

// Fetch and parse a trace (server gunzips and concatenates segments)
const keys = objects.map(o => `keys=${encodeURIComponent(o.key)}`).join('&');
const traceResp = await fetch(`http://localhost:3000/api/trace?bucket=BUCKET&${keys}`);
const buf = Buffer.from(await traceResp.arrayBuffer());
const trace = await parseTrace(buf);
```

## Available skill segments

Run `dial9-viewer agents <segment>` for detailed information:

| Command / Segment | Description |
|-------------------|-------------|
| `agents toolkit --path DIR` | **Start here.** Copies the analysis toolkit to a directory |
| `agents skill runtime` | Tokio runtime internals: execution model, scheduling, wake/poll lifecycle, and how to fix common problems |
| `agents skill loading` | Trace format details, parsing options, time range filtering |
| `agents skill analysis` | Full analysis pipeline API reference |
| `agents skill recipes` | Diagnostic recipes for common questions |
| `agents skill red-flags` | Automated checks for common runtime problems |
