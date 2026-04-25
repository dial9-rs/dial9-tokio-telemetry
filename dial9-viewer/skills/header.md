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
- **Span events**: Enter/exit events from `tracing` spans (`#[instrument]`), showing what happened inside each poll with field values and nesting

## Instrumenting your app

Run `dial9-viewer agents skill setup` for full setup instructions (prerequisites, macro and manual setup, tracing layer, wake tracking).

## Quick start (analysis)

Get the analysis toolkit:

```bash
dial9-viewer agents toolkit /tmp/d9-toolkit
node /tmp/d9-toolkit/analyze.js <trace.bin>
node /tmp/d9-toolkit/analyze.js <directory-of-traces/>
```

Run `analyze.js` for a full diagnostic report.

For directories, use `--force` to re-parse all files without caching, or `--sample N` to analyze only N evenly-spaced files.

### Parsing traces manually

```javascript
const { parseTrace, EVENT_TYPES } = require('./trace_parser.js');
const { buildWorkerSpans, attachCpuSamples } = require('./trace_analysis.js');

for await (const trace of parseTrace('/path/to/traces/')) {
  const workerIds = [...new Set(
    trace.events.filter(e => e.eventType !== EVENT_TYPES.QueueSample && e.eventType !== EVENT_TYPES.WakeEvent)
      .map(e => e.workerId)
  )].sort((a, b) => a - b);
  const maxTs = trace.events.reduce((m, e) => Math.max(m, e.timestamp), -Infinity);
  const spans = buildWorkerSpans(trace.events, workerIds, maxTs);
  attachCpuSamples(trace.cpuSamples, spans.workerSpans);
}
```

Pass a file path or a directory. For directories with more than a few hundred files, use `--sample 100` for an initial scan, then run without sampling for the full picture. Options: `{ force: true }` to ignore cache, `{ sample: N }` to parse only N evenly-spaced files.

## Fetching traces from S3

If `dial9-viewer` is running (e.g. on port 3000), fetch traces via its API:

```javascript
// List traces matching a prefix
const resp = await fetch('http://localhost:3000/api/search?bucket=BUCKET&q=2026-04-09/19');
const objects = await resp.json(); // [{key, size, last_modified}, ...]

// Single file: fetch and parse one trace
const traceResp = await fetch(`http://localhost:3000/api/trace?bucket=BUCKET&keys=${encodeURIComponent(objects[0].key)}`);
const buf = Buffer.from(await traceResp.arrayBuffer());
const trace = await parseTrace(buf);

// Multiple files: download to a local directory, then analyze
const fs = require('fs');
const dir = '/tmp/traces';
fs.mkdirSync(dir, { recursive: true });
// Download in parallel (20 at a time)
const limit = 20;
for (let i = 0; i < objects.length; i += limit) {
  await Promise.all(objects.slice(i, i + limit).map(async (obj) => {
    const r = await fetch(`http://localhost:3000/api/trace?bucket=BUCKET&keys=${encodeURIComponent(obj.key)}`);
    fs.writeFileSync(`${dir}/${obj.key.split('/').pop()}`, Buffer.from(await r.arrayBuffer()));
  }));
}
for await (const trace of parseTrace(dir)) {
  // analyze each trace
}
```

## Available skill segments

Run `dial9-viewer agents <segment>` for detailed information:

| Command / Segment | Description |
|-------------------|-------------|
| `agents toolkit DIR` | **Start here.** Copies the analysis toolkit to a directory |
| `agents skill setup` | How to instrument your app with dial9 and the tracing layer (from README) |
| `agents skill runtime` | Tokio runtime internals: execution model, scheduling, wake/poll lifecycle, and how to fix common problems |
| `agents skill loading` | Trace format details, parsing options, time range filtering |
| `agents skill analysis` | Full analysis pipeline API reference |
| `agents skill recipes` | Diagnostic recipes for common questions |
| `agents skill red-flags` | Automated checks for common runtime problems |
