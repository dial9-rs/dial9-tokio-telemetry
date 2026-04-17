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
