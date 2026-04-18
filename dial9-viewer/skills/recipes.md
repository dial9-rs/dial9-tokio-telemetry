# Diagnostic Recipes

Concrete code snippets for answering common questions about trace data. All recipes assume the standard pipeline has been run (see `analysis` segment).

## Setup boilerplate

```javascript
const fs = require('fs');
const { parseTrace, EVENT_TYPES, formatFrame, symbolizeChain, deduplicateSamples } = require('./trace_parser.js');
const { buildWorkerSpans, attachCpuSamples, buildActiveTaskTimeline,
        computeSchedulingDelays, filterPointsOfInterest, buildFgData } = require('./trace_analysis.js');

async function analyze(tracePath) {
  const trace = await parseTrace(fs.readFileSync(tracePath));
  const workerIds = [...new Set(
    trace.events.filter(e => e.eventType !== EVENT_TYPES.QueueSample && e.eventType !== EVENT_TYPES.WakeEvent)
      .map(e => e.workerId)
  )].sort((a, b) => a - b);
  const maxTs = trace.events.reduce((m, e) => Math.max(m, e.timestamp), -Infinity);
  const minTs = trace.events.reduce((m, e) => Math.min(m, e.timestamp), Infinity);
  const spans = buildWorkerSpans(trace.events, workerIds, maxTs);
  attachCpuSamples(trace.cpuSamples, spans.workerSpans);
  const taskTimeline = buildActiveTaskTimeline(trace.taskSpawnTimes, trace.taskTerminateTimes);
  const schedDelays = computeSchedulingDelays(spans.workerSpans, workerIds, spans.wakesByTask);
  return { trace, workerIds, minTs, maxTs, spans, taskTimeline, schedDelays };
}
```

## Which task has the longest poll time?

```javascript
let worst = null;
for (const w of workerIds) {
  for (const p of spans.workerSpans[w].polls) {
    const dur = p.end - p.start;
    if (!worst || dur > worst.dur) worst = { dur, poll: p, worker: w };
  }
}
if (worst) {
  const ms = worst.dur / 1e6;
  const relStart = (worst.poll.start - minTs) / 1e6;
  console.log(`Longest poll: ${ms.toFixed(2)}ms at ${relStart.toFixed(1)}ms`);
  console.log(`  Task ID: ${worst.poll.taskId}, Spawn: ${worst.poll.spawnLoc}`);
  if (worst.poll.cpuSamples?.length) {
    console.log(`  CPU samples during this poll:`);
    for (const s of worst.poll.cpuSamples) {
      const frames = symbolizeChain(s.callchain, trace.callframeSymbols);
      console.log(`    ${formatFrame(frames[0]).text}`);
    }
  }
  if (worst.poll.schedSamples?.length) {
    console.log(`  Scheduling (blocking) samples during this poll:`);
    for (const s of worst.poll.schedSamples) {
      const frames = symbolizeChain(s.callchain, trace.callframeSymbols);
      console.log(`    ${formatFrame(frames[0]).text}`);
    }
  }
}
```

## Do I have a task leak?

A task leak means tasks are spawned but never terminate, causing the active count to grow monotonically.

```javascript
const samples = taskTimeline.activeTaskSamples;
if (samples.length > 0) {
  const first = samples[0].count;
  const last = samples[samples.length - 1].count;
  const peak = Math.max(...samples.map(s => s.count));
  console.log(`Active tasks: start=${first}, end=${last}, peak=${peak}`);

  // Check if active count is monotonically increasing (never decreases)
  let monotonic = true;
  for (let i = 1; i < samples.length; i++) {
    if (samples[i].count < samples[i - 1].count) { monotonic = false; break; }
  }
  if (monotonic && last > first * 2) {
    console.log('⚠ Possible task leak: active count grew monotonically');
  } else if (last > first * 2 && last === peak) {
    console.log('⚠ Active count grew but is not strictly monotonic — may be ramp-up in a short trace');
  }

  // Find which spawn locations have the most unterminated tasks
  const alive = new Map();
  for (const [taskId, spawnTime] of trace.taskSpawnTimes) {
    if (!trace.taskTerminateTimes.has(taskId)) {
      const loc = trace.taskSpawnLocs.get(taskId) || '(unknown)';
      alive.set(loc, (alive.get(loc) || 0) + 1);
    }
  }
  console.log('Unterminated tasks by spawn location:');
  for (const [loc, count] of [...alive.entries()].sort((a, b) => b[1] - a[1])) {
    console.log(`  ${count} tasks from ${loc}`);
  }
}
```

## Task spawn rate by location

```javascript
const spawnCounts = new Map();
for (const [taskId, loc] of trace.taskSpawnLocs) {
  spawnCounts.set(loc || '(unknown)', (spawnCounts.get(loc || '(unknown)') || 0) + 1);
}
console.log('Tasks spawned per location:');
for (const [loc, count] of [...spawnCounts.entries()].sort((a, b) => b[1] - a[1])) {
  console.log(`  ${count} from ${loc}`);
}
```

## Flamegraph for a specific spawn location

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

Note: `spawnLoc` is set on samples by `attachCpuSamples()` — you must call it first.

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
const longPolls = filterPointsOfInterest('long-poll', spans.workerSpans, workerIds, schedDelays, { hasSchedWait: true, sortByWorst: true });
console.log(`${longPolls.length} polls longer than 1ms`);

// Summarize by spawn location
const byLoc = new Map();
for (const lp of longPolls) {
  const loc = lp.span.spawnLoc || '(unknown)';
  const entry = byLoc.get(loc) || { count: 0, totalMs: 0, maxMs: 0 };
  entry.count++;
  entry.totalMs += lp.value;
  entry.maxMs = Math.max(entry.maxMs, lp.value);
  byLoc.set(loc, entry);
}
console.log('Long polls by spawn location:');
for (const [loc, e] of [...byLoc.entries()].sort((a, b) => b.totalMs - a.totalMs)) {
  console.log(`  ${loc}: ${e.count} polls, total=${e.totalMs.toFixed(1)}ms, max=${e.maxMs.toFixed(1)}ms`);
}

// Check if long polls correlate with high scheduling delays
const highDelays = schedDelays.filter(d => d.delay > 1e6); // >1ms
console.log(`\n${highDelays.length} scheduling delays > 1ms`);
if (highDelays.length > 0) {
  const maxDelay = Math.max(...highDelays.map(d => d.delay));
  console.log(`Worst scheduling delay: ${(maxDelay / 1e6).toFixed(2)}ms`);
  console.log('This means tasks were woken but had to wait for a worker — workers were busy with long polls.');
}
```

## Worker utilization

```javascript
for (const w of workerIds) {
  const actives = spans.workerSpans[w].actives;
  const parks = spans.workerSpans[w].parks;
  const totalActiveNs = actives.reduce((s, a) => s + (a.end - a.start), 0);
  const totalParkNs = parks.reduce((s, p) => s + (p.end - p.start), 0);
  const totalNs = totalActiveNs + totalParkNs;
  const utilization = totalNs > 0 ? totalActiveNs / totalNs : 0;
  const avgCpuRatio = actives.length > 0
    ? actives.reduce((s, a) => s + a.ratio, 0) / actives.length : 0;
  console.log(`Worker ${w}: ${(utilization * 100).toFixed(1)}% active, avg CPU ratio ${avgCpuRatio.toFixed(3)}`);
}
```

## Blocking call detection

Scheduling samples (source=1) capture stack traces when the OS deschedules a worker thread. These reveal blocking calls (file I/O, DNS, locks, etc.).

```javascript
const schedSamples = trace.cpuSamples.filter(s => s.source === 1);
if (schedSamples.length > 0) {
  const groups = deduplicateSamples(schedSamples, trace.callframeSymbols);
  console.log(`${schedSamples.length} scheduling (off-CPU) samples — these show blocking calls:`);
  for (const g of groups.slice(0, 10)) {
    console.log(`  ${g.count} samples — ${g.leaf}`);
    // Print full stack for the top offender
    if (g === groups[0]) {
      console.log('  Full stack:');
      for (const f of g.frames) {
        console.log(`    ${formatFrame(f).text}`);
      }
    }
  }
}
```

## Wake chain analysis

Trace the chain of wakes that led to a specific task being polled:

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

// Example: pick a task ID of interest and trace its wake chain
const taskId = 42; // replace with a task ID from your trace
traceWakeChain(taskId, spans.wakesByTask, trace.taskSpawnLocs);
```

---

# Span Recipes

Requires `Dial9TokioLayer` in the subscriber (see `tracing-layer` feature).

## What spans happened inside a long poll?

Requires `Dial9TokioLayer` in the subscriber (see `tracing-layer` feature).

```javascript
const { buildSpanData } = require('./trace_analysis.js');
const { spansByWorker } = buildSpanData(trace.customEvents);

// Find the longest poll
let worst = null;
for (const w of workerIds) {
  for (const p of spans.workerSpans[w].polls) {
    const dur = p.end - p.start;
    if (!worst || dur > worst.dur) worst = { dur, poll: p, worker: w };
  }
}

// Find spans within that poll
const wSpans = spansByWorker[worst.worker] || [];
const inner = wSpans.filter(s => s.start >= worst.poll.start && s.end <= worst.poll.end);
console.log(`Longest poll: ${(worst.dur / 1e6).toFixed(2)}ms on worker ${worst.worker}`);
console.log(`Contains ${inner.length} spans:`);
const byName = {};
for (const s of inner) byName[s.spanName] = (byName[s.spanName] || 0) + 1;
for (const [name, count] of Object.entries(byName)) {
  console.log(`  ${name}: ${count}`);
}
```

## Span duration percentiles by name

```javascript
const { buildSpanData } = require('./trace_analysis.js');
const { spansByWorker } = buildSpanData(trace.customEvents);

const allSpans = Object.values(spansByWorker).flat();
const byName = {};
for (const s of allSpans) {
  (byName[s.spanName] ??= []).push(s.end - s.start);
}
for (const [name, durations] of Object.entries(byName)) {
  durations.sort((a, b) => a - b);
  const p50 = durations[Math.floor(durations.length * 0.5)];
  const p99 = durations[Math.floor(durations.length * 0.99)];
  console.log(`${name}: count=${durations.length} p50=${(p50/1e3).toFixed(1)}µs p99=${(p99/1e3).toFixed(1)}µs`);
}
```

## Filter spans by field value

```javascript
const { buildSpanData } = require('./trace_analysis.js');
const { spansByWorker } = buildSpanData(trace.customEvents);

const allSpans = Object.values(spansByWorker).flat();
const matches = allSpans.filter(s => s.fields.request_id === 'abc-123');
console.log(`${matches.length} spans for request abc-123:`);
for (const s of matches) {
  console.log(`  ${s.spanName} ${(( s.end - s.start) / 1e3).toFixed(1)}µs`);
}
```

## How many spans per poll? (detect tight loops)

```javascript
const { buildSpanData } = require('./trace_analysis.js');
const { spansByWorker } = buildSpanData(trace.customEvents);

for (const w of workerIds) {
  for (const p of spans.workerSpans[w].polls) {
    const wSpans = spansByWorker[w] || [];
    const inner = wSpans.filter(s => s.start >= p.start && s.end <= p.end);
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
const { spansByWorker } = buildSpanData(trace.customEvents);

// Find the slowest query_metric span
const allSpans = Object.values(spansByWorker).flat();
const slowest = allSpans
  .filter(s => s.spanName === 'query_metric')
  .sort((a, b) => (b.end - b.start) - (a.end - a.start))[0];

if (slowest) {
  console.log(`Slowest query_metric: ${((slowest.end - slowest.start) / 1e6).toFixed(2)}ms`);
  console.log(`Fields: ${JSON.stringify(slowest.fields)}`);

  // What other spans overlapped on all workers?
  for (const [w, wSpans] of Object.entries(spansByWorker)) {
    const overlapping = wSpans.filter(s => s.start < slowest.end && s.end > slowest.start && s !== slowest);
    if (overlapping.length > 0) {
      const byName = {};
      for (const s of overlapping) byName[s.spanName] = (byName[s.spanName] || 0) + 1;
      console.log(`  Worker ${w}: ${Object.entries(byName).map(([n, c]) => `${n}×${c}`).join(', ')}`);
    }
  }
}
```

## Where does a specific span rank among its peers?

Given a span, show its percentile rank compared to all spans of the same name.

```javascript
const { buildSpanData } = require('./trace_analysis.js');
const { spansByWorker } = buildSpanData(trace.customEvents);

function spanPercentile(span) {
  const allSpans = Object.values(spansByWorker).flat();
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
const allSpans = Object.values(spansByWorker).flat();
const slowest = allSpans
  .filter(s => s.spanName === 'query_metric')
  .sort((a, b) => (b.end - b.start) - (a.end - a.start))[0];
if (slowest) spanPercentile(slowest);
```

## Trace a request across workers

Show the full timeline of a request by field value, including which workers handled it.

```javascript
const { buildSpanData } = require('./trace_analysis.js');
const { spansByWorker } = buildSpanData(trace.customEvents);

const requestId = 'abc-123'; // replace with your request ID
const timeline = [];
for (const [w, wSpans] of Object.entries(spansByWorker)) {
  for (const s of wSpans) {
    if (s.fields.request_id === requestId) {
      timeline.push({ ...s, worker: Number(w) });
    }
  }
}
timeline.sort((a, b) => a.start - b.start);
for (const s of timeline) {
  console.log(`  +${((s.start - minTs) / 1e6).toFixed(3)}ms worker=${s.worker} ${s.spanName} ${((s.end - s.start) / 1e3).toFixed(1)}µs`);
}
```
