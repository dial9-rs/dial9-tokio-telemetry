---
name: dial9-trace-recipes
description: Diagnostic recipes for common questions about dial9 Tokio runtime traces. Covers finding long polls, task leaks, worker utilization, blocking calls, wake chains, span analysis, task dumps, and time-window debugging. Use when answering specific diagnostic questions about trace data.
---

# Diagnostic Recipes

Concrete code snippets for answering common questions about trace data.

## Setup boilerplate

Two APIs depending on what you need:

**`analyzeTraces(path)`** returns aggregated results across all files (parallel, fast). Use for diagnostic questions like "what's the worst poll" or "what's the utilization."

```javascript
const { analyzeTraces } = require('./analyze.js');
const result = await analyzeTraces('/path/to/traces/');
// result.longPolls, result.workerSpans, result.schedDelayHist, result.cpuGroups, etc.
```

**`parseTrace(path)`** yields one `ParsedTrace` per file. Use when you need raw per-trace data (flamegraphs, field filtering, wake chains).

```javascript
const { parseTrace, EVENT_TYPES, formatFrame, symbolizeChain, deduplicateSamples } = require('./trace_parser.js');
const { buildWorkerSpans, attachCpuSamples,
        computeSchedulingDelays } = require('./trace_analysis.js');

for await (const trace of parseTrace('/path/to/traces/')) {
  const workerIds = [...new Set(
    trace.events.filter(e => e.eventType !== EVENT_TYPES.QueueSample && e.eventType !== EVENT_TYPES.WakeEvent)
      .map(e => e.workerId)
  )].sort((a, b) => a - b);
  const maxTs = trace.maxTs;
  const minTs = trace.minTs;
  const spans = buildWorkerSpans(trace.events, workerIds, maxTs);
  attachCpuSamples(trace.cpuSamples, spans.workerSpans);
  const schedDelays = computeSchedulingDelays(spans.workerSpans, workerIds, spans.wakesByTask);
}
```

## Which task has the longest poll time?

```javascript
const { analyzeTraces } = require('./analyze.js');
const { symbolizeChain } = require('./trace_parser.js');
const result = await analyzeTraces('/path/to/traces/');
const worst = result.longPolls[0];
if (worst) {
  console.log(`Longest poll: ${(worst.dur / 1e6).toFixed(2)}ms`);
  console.log(`  Task ID: ${worst.poll.taskId}, Spawn: ${worst.poll.spawnLoc}`);
  if (worst.poll.cpuSamples?.length) {
    for (const s of worst.poll.cpuSamples) {
      const frames = symbolizeChain(s.callchain, result.callframeSymbols);
      console.log(`  CPU: ${require('./trace_parser.js').formatFrame(frames[0]).text}`);
    }
  }
  if (worst.poll.schedSamples?.length) {
    for (const s of worst.poll.schedSamples) {
      const frames = symbolizeChain(s.callchain, result.callframeSymbols);
      console.log(`  Sched: ${require('./trace_parser.js').formatFrame(frames[0]).text}`);
    }
  }
}
```

## Do I have a task leak?

A task leak means tasks are spawned but never terminate, causing the active count to grow monotonically.

```javascript
const { analyzeTraces } = require('./analyze.js');
const result = await analyzeTraces('/path/to/traces/');
const samples = result.taskTimeline.activeTaskSamples;
if (samples.length > 0) {
  const first = samples[0].count;
  const last = samples[samples.length - 1].count;
  const peak = samples.reduce((m, s) => Math.max(m, s.count), -Infinity);
  console.log(`Active tasks: start=${first}, end=${last}, peak=${peak}`);
  if (last > first * 2 && last === peak) {
    console.log('⚠ Possible task leak');
    const alive = new Map();
    for (const [taskId] of result.taskSpawnTimes) {
      if (!result.taskTerminateTimes.has(taskId)) {
        const loc = result.taskSpawnLocs.get(taskId) || '(unknown)';
        alive.set(loc, (alive.get(loc) || 0) + 1);
      }
    }
    for (const [loc, count] of [...alive.entries()].sort((a, b) => b[1] - a[1])) {
      console.log(`  ${count} tasks from ${loc}`);
    }
  }
}
```

## Task spawn rate by location

```javascript
const { analyzeTraces } = require('./analyze.js');
const result = await analyzeTraces('/path/to/traces/');
const spawnCounts = new Map();
for (const [, loc] of result.taskSpawnLocs) {
  spawnCounts.set(loc || '(unknown)', (spawnCounts.get(loc || '(unknown)') || 0) + 1);
}
for (const [loc, count] of [...spawnCounts.entries()].sort((a, b) => b[1] - a[1])) {
  console.log(`  ${count} from ${loc}`);
}
```

## Flamegraph for a specific spawn location

Requires per-trace iteration (see `parseTrace` boilerplate above).

```javascript
const targetLoc = 'src/main.rs:42:5'; // adjust to your spawn location
const targetSamples = trace.cpuSamples.filter(s => s.spawnLoc === targetLoc);
console.log(`${targetSamples.length} CPU samples for tasks from ${targetLoc}`);

const groups = deduplicateSamples(targetSamples, trace.callframeSymbols);
console.log('Top hotspots:');
for (const g of groups.slice(0, 10)) {
  console.log(`  ${g.count} samples (${(g.count/targetSamples.length*100).toFixed(1)}%) — ${g.leaf}`);
}
```

Note: `spawnLoc` is set on samples by `attachCpuSamples()`.

## What's happening at a specific time?

```javascript
const targetMs = 1500; // 1.5 seconds into the trace
const targetNs = minTs + targetMs * 1e6;
const windowNs = 10 * 1e6; // ±10ms window

for (const w of workerIds) {
  const polls = spans.workerSpans[w].polls.filter(p =>
    p.end >= targetNs - windowNs && p.start <= targetNs + windowNs
  );
  console.log(`Worker ${w}: ${polls.length} polls in window`);
  for (const p of polls) {
    const dur = (p.end - p.start) / 1e6;
    const rel = (p.start - minTs) / 1e6;
    console.log(`  ${rel.toFixed(1)}ms +${dur.toFixed(2)}ms task=${p.taskId} spawn=${p.spawnLoc}`);
  }
}

// Check queue depth at that time
if (spans.queueSamples.length > 0) {
  const nearestQueue = spans.queueSamples.reduce((best, s) =>
    Math.abs(s.t - targetNs) < Math.abs(best.t - targetNs) ? s : best
  );
  console.log(`Queue depth near target: global=${nearestQueue.global}`);
}
```

## Are long poll times hurting my application?

```javascript
const { analyzeTraces } = require('./analyze.js');
const result = await analyzeTraces('/path/to/traces/');
console.log(`${result.longPolls.length} long polls (>1ms)`);
// Poll duration by spawn location
for (const [loc, h] of result.pollDurationByLoc) {
  console.log(`  ${loc}: p50=${(h.percentile(50)/1e3).toFixed(1)}µs p99=${(h.percentile(99)/1e3).toFixed(1)}µs max=${(h.max/1e6).toFixed(2)}ms`);
}
// Scheduling delay correlation
if (result.schedDelayHist) {
  console.log(`Scheduling delays: p99=${(result.schedDelayHist.percentile(99)/1e6).toFixed(2)}ms max=${(result.schedDelayHist.max/1e6).toFixed(2)}ms`);
}
```

## Worker utilization

```javascript
const { analyzeTraces } = require('./analyze.js');
const result = await analyzeTraces('/path/to/traces/');
for (const w of result.workerIds) {
  const ws = result.workerSpans[w];
  console.log(`Worker ${w}: ${(ws.utilization * 100).toFixed(1)}% active, avg CPU ratio ${ws.avgCpuRatio.toFixed(3)}`);
}
```

## Blocking call detection

Scheduling samples (source=1) capture stack traces when the OS deschedules a worker thread.

```javascript
const { analyzeTraces } = require('./analyze.js');
const { formatFrame } = require('./trace_parser.js');
const result = await analyzeTraces('/path/to/traces/');
console.log(`${result.offCpuSampleCount} off-CPU samples`);
for (const g of result.schedGroups.slice(0, 10)) {
  console.log(`  ${g.count} samples — ${g.leaf}`);
  if (g === result.schedGroups[0]) {
    for (const f of g.frames) console.log(`    ${formatFrame(f).text}`);
  }
}
```

## Wake chain analysis

```javascript
function traceWakeChain(taskId, wakesByTask, taskSpawnLocs, depth = 0, seen = new Set()) {
  if (seen.has(taskId)) return;
  seen.add(taskId);
  const wakes = wakesByTask[taskId];
  if (!wakes || wakes.length === 0) return;
  const lastWake = wakes[wakes.length - 1];
  const loc = taskSpawnLocs.get(taskId) || '(unknown)';
  console.log(`${'  '.repeat(depth)}Task ${taskId} (${loc}) woken by task ${lastWake.wakerTaskId}`);
  if (depth < 5) traceWakeChain(lastWake.wakerTaskId, wakesByTask, taskSpawnLocs, depth + 1, seen);
}

// Example: trace a task's wake chain
const taskId = 42; // replace with a task ID from your trace
traceWakeChain(taskId, spans.wakesByTask, trace.taskSpawnLocs);
```

## Span duration percentiles by name

Requires `Dial9TokioLayer` in the subscriber.

```javascript
const { analyzeTraces } = require('./analyze.js');
const result = await analyzeTraces('/path/to/traces/');
for (const [name, h] of result.spanStats) {
  console.log(`${name}: count=${h.count} p50=${(h.percentile(50)/1e3).toFixed(1)}µs p99=${(h.percentile(99)/1e3).toFixed(1)}µs max=${(h.max/1e3).toFixed(1)}µs`);
}
```

## What spans happened inside a long poll?

```javascript
const { buildSpanData } = require('./trace_analysis.js');
const { allSpans } = buildSpanData(trace.customEvents);

// Find the longest poll
let worst = null;
for (const w of workerIds) {
  for (const p of spans.workerSpans[w].polls) {
    const dur = p.end - p.start;
    if (!worst || dur > worst.dur) worst = { dur, poll: p, worker: w };
  }
}

// Find spans that overlap with that poll (via segments on the same worker)
const inner = allSpans.filter(s =>
  s.segments.some(seg => seg.workerId === worst.worker && seg.start >= worst.poll.start && seg.end <= worst.poll.end)
);
console.log(`Longest poll: ${(worst.dur / 1e6).toFixed(2)}ms on worker ${worst.worker}`);
console.log(`Contains ${inner.length} spans:`);
const byName = {};
for (const s of inner) byName[s.spanName] = (byName[s.spanName] || 0) + 1;
for (const [name, count] of Object.entries(byName)) {
  console.log(`  ${name}: ${count}`);
}
```

## Filter spans by field value

```javascript
const { buildSpanData } = require('./trace_analysis.js');
const { allSpans } = buildSpanData(trace.customEvents);

const matches = allSpans.filter(s => s.fields.request_id === 'abc-123');
console.log(`${matches.length} spans for request abc-123:`);
for (const s of matches) {
  console.log(`  ${s.spanName} ${(( s.end - s.start) / 1e3).toFixed(1)}µs`);
}
```

## Detect tight loops (many spans per poll)

```javascript
const { buildSpanData } = require('./trace_analysis.js');
const { allSpans } = buildSpanData(trace.customEvents);

for (const w of workerIds) {
  for (const p of spans.workerSpans[w].polls) {
    const inner = allSpans.filter(s =>
      s.segments.some(seg => seg.workerId === w && seg.start >= p.start && seg.end <= p.end)
    );
    if (inner.length > 10) {
      const byName = {};
      for (const s of inner) byName[s.spanName] = (byName[s.spanName] || 0) + 1;
      const summary = Object.entries(byName).map(([n, c]) => `${n}×${c}`).join(', ');
      console.log(`Worker ${w} poll at +${((p.start - minTs) / 1e6).toFixed(1)}ms: ${inner.length} spans (${summary}), poll duration ${((p.end - p.start) / 1e6).toFixed(2)}ms`);
    }
  }
}
```

## What else was happening during a slow span?

Find a slow span and see what other spans and polls overlap on the same and other workers.

```javascript
const { buildSpanData } = require('./trace_analysis.js');
const { allSpans } = buildSpanData(trace.customEvents);

// Find the slowest query_metric span
const slowest = allSpans
  .filter(s => s.spanName === 'query_metric')
  .sort((a, b) => (b.end - b.start) - (a.end - a.start))[0];

if (slowest) {
  console.log(`Slowest query_metric: ${((slowest.end - slowest.start) / 1e6).toFixed(2)}ms`);
  console.log(`Fields: ${JSON.stringify(slowest.fields)}`);

  // What other spans overlapped?
  const overlapping = allSpans.filter(s => s.start < slowest.end && s.end > slowest.start && s !== slowest);
  const byName = {};
  for (const s of overlapping) byName[s.spanName] = (byName[s.spanName] || 0) + 1;
  for (const [name, count] of Object.entries(byName)) {
    console.log(`  ${name}: ${count}`);
  }
}
```

## Where does a specific span rank among its peers?

Given a span, show its percentile rank compared to all spans of the same name.

```javascript
const { buildSpanData } = require('./trace_analysis.js');
const { allSpans } = buildSpanData(trace.customEvents);

function spanPercentile(span) {
  const peers = allSpans.filter(s => s.spanName === span.spanName).map(s => s.end - s.start);
  peers.sort((a, b) => a - b);
  const dur = span.end - span.start;
  const rank = peers.filter(d => d <= dur).length;
  const pct = (rank / peers.length * 100).toFixed(1);
  const p50 = peers[Math.floor(peers.length * 0.5)];
  const p90 = peers[Math.floor(peers.length * 0.9)];
  const p99 = peers[Math.floor(peers.length * 0.99)];
  console.log(`${span.spanName} duration: ${(dur / 1e3).toFixed(1)}µs (P${pct} of ${peers.length})`);
  console.log(`  p0=${(peers[0] / 1e3).toFixed(1)}µs p50=${(p50 / 1e3).toFixed(1)}µs p90=${(p90 / 1e3).toFixed(1)}µs p99=${(p99 / 1e3).toFixed(1)}µs p100=${(peers[peers.length - 1] / 1e3).toFixed(1)}µs`);
}

// Example: rank the slowest query_metric
const slowest = allSpans
  .filter(s => s.spanName === 'query_metric')
  .sort((a, b) => (b.end - b.start) - (a.end - a.start))[0];
if (slowest) spanPercentile(slowest);
```

## Trace a request across workers

```javascript
const { buildSpanData } = require('./trace_analysis.js');
const { allSpans } = buildSpanData(trace.customEvents);

const requestId = 'abc-123'; // replace with your request ID
const matches = allSpans.filter(s => s.fields.request_id === requestId);
matches.sort((a, b) => a.start - b.start);
for (const s of matches) {
  const workers = [...new Set(s.segments.map(seg => seg.workerId))].join(',');
  console.log(`  +${((s.start - minTs) / 1e6).toFixed(3)}ms worker=${workers} ${s.spanName} ${((s.end - s.start) / 1e3).toFixed(1)}µs`);
}
```

## What is a task waiting on? (task dumps)

Task dumps capture async backtraces at yield points. Use them to see what futures a task is `await`ing during idle periods.

```javascript
const { parseTrace, symbolizeChain, formatFrame } = require('./trace_parser.js');
const { buildWorkerSpans } = require('./trace_analysis.js');

for await (const trace of parseTrace('/path/to/traces/')) {
  const workerIds = [...new Set(trace.events.filter(e => e.workerId !== undefined).map(e => e.workerId))].sort((a,b)=>a-b);
  const spans = buildWorkerSpans(trace.events, workerIds, trace.maxTs);

  for (const [taskId, dumps] of trace.taskDumps) {
    const taskPolls = [];
    for (const w of workerIds) {
      for (const p of spans.workerSpans[w].polls) {
        if (p.taskId === taskId) taskPolls.push(p);
      }
    }
    taskPolls.sort((a, b) => a.start - b.start);

    const loc = trace.taskSpawnLocs?.get(taskId) || '(unknown)';
    for (const dump of dumps) {
      // dump.timestamp matches the pollStart of the following poll;
      // the idle period is the gap between the preceding poll's end and this timestamp
      const pollIdx = taskPolls.findIndex(p => p.start === dump.timestamp);
      const idleStart = pollIdx > 0 ? taskPolls[pollIdx - 1].end : trace.minTs;
      const idleDur = (dump.timestamp - idleStart) / 1e6;

      const frames = symbolizeChain(dump.callchain, trace.callframeSymbols);
      const leaf = frames[0] ? formatFrame(frames[0]).text : '(unknown)';
      console.log(`Task ${taskId} (${loc}) idle ${idleDur.toFixed(1)}ms awaiting: ${leaf}`);
    }
  }
}
```

