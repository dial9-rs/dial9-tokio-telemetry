# Red Flags — Automated Health Checks

Run these checks against any trace to surface common Tokio runtime problems. Each check prints a finding with severity.

## Complete red-flag scan

```javascript
const fs = require('fs');
const { parseTrace, EVENT_TYPES, deduplicateSamples } = require('./trace_parser.js');
const { buildWorkerSpans, attachCpuSamples, buildActiveTaskTimeline,
        computeSchedulingDelays, buildSpanData } = require('./trace_analysis.js');

async function redFlagScan(tracePath) {
  const trace = await parseTrace(fs.readFileSync(tracePath));
  const workerIds = [...new Set(
    trace.events.filter(e => e.eventType !== EVENT_TYPES.QueueSample && e.eventType !== EVENT_TYPES.WakeEvent)
      .map(e => e.workerId)
  )].sort((a, b) => a - b);
  const maxTs = trace.events.reduce((m, e) => Math.max(m, e.timestamp), -Infinity);
  const minTs = trace.events.reduce((m, e) => Math.min(m, e.timestamp), Infinity);
  const durationMs = (maxTs - minTs) / 1e6;
  const spans = buildWorkerSpans(trace.events, workerIds, maxTs);
  attachCpuSamples(trace.cpuSamples, spans.workerSpans);
  const taskTimeline = buildActiveTaskTimeline(trace.taskSpawnTimes, trace.taskTerminateTimes);
  const schedDelays = computeSchedulingDelays(spans.workerSpans, workerIds, spans.wakesByTask);

  const findings = [];

  // 1. Long polls (blocking the runtime)
  for (const w of workerIds) {
    for (const p of spans.workerSpans[w].polls) {
      const durMs = (p.end - p.start) / 1e6;
      if (durMs > 50) {
        findings.push({
          severity: 'critical',
          check: 'long-poll',
          message: `Poll of ${durMs.toFixed(1)}ms on worker ${w} at ${((p.start - minTs) / 1e6).toFixed(1)}ms (task ${p.taskId}, spawn: ${p.spawnLoc})`,
        });
      } else if (durMs > 10) {
        findings.push({
          severity: 'warning',
          check: 'long-poll',
          message: `Poll of ${durMs.toFixed(1)}ms on worker ${w} at ${((p.start - minTs) / 1e6).toFixed(1)}ms (task ${p.taskId}, spawn: ${p.spawnLoc})`,
        });
      }
    }
  }

  // 2. Task leak detection
  const samples = taskTimeline.activeTaskSamples;
  if (samples.length > 10) {
    const first = samples[0].count;
    const last = samples[samples.length - 1].count;
    const peak = samples.reduce((m, s) => Math.max(m, s.count), -Infinity);
    if (last > first * 2 && last === peak) {
      findings.push({
        severity: 'warning',
        check: 'task-leak',
        message: `Active task count grew from ${first} to ${last} (peak ${peak}) — possible task leak`,
      });
    }
  }

  // 3. High scheduling delays (tasks waiting for workers)
  const highDelays = schedDelays.filter(d => d.delay > 5e6); // >5ms
  if (highDelays.length > 0) {
    const worst = highDelays.reduce((m, d) => Math.max(m, d.delay), -Infinity);
    findings.push({
      severity: worst > 50e6 ? 'critical' : 'warning',
      check: 'sched-delay',
      message: `${highDelays.length} scheduling delays > 5ms (worst: ${(worst / 1e6).toFixed(1)}ms) — tasks waiting for busy workers`,
    });
  }

  // 4. Blocking calls detected via scheduling samples
  const schedSamples = trace.cpuSamples.filter(s => s.source === 1);
  if (schedSamples.length > 0) {
    const groups = deduplicateSamples(schedSamples, trace.callframeSymbols);
    const topBlocker = groups[0];
    if (topBlocker && topBlocker.count > 5) {
      findings.push({
        severity: 'warning',
        check: 'blocking-calls',
        message: `${schedSamples.length} off-CPU samples detected. Top blocker: "${topBlocker.leaf}" (${topBlocker.count} samples)`,
      });
    }
  }

  // 5. Global queue buildup
  const highQueue = spans.queueSamples.filter(s => s.global > 100);
  if (highQueue.length > 0) {
    const maxQueue = spans.queueSamples.reduce((m, s) => Math.max(m, s.global), -Infinity);
    findings.push({
      severity: maxQueue > 1000 ? 'critical' : 'warning',
      check: 'queue-depth',
      message: `Global queue reached ${maxQueue} (${highQueue.length} samples > 100) — runtime is overloaded`,
    });
  }

  // 6. Worker imbalance
  if (workerIds.length > 1) {
    const pollCounts = workerIds.map(w => spans.workerSpans[w].polls.length);
    const max = pollCounts.reduce((m, c) => Math.max(m, c), -Infinity);
    const min = pollCounts.reduce((m, c) => Math.min(m, c), Infinity);
    if (max > min * 3 && min > 0) {
      findings.push({
        severity: 'info',
        check: 'worker-imbalance',
        message: `Worker poll imbalance: ${min}–${max} polls across workers (${(max/min).toFixed(1)}x ratio)`,
      });
    }
  }

  // 7. Low CPU utilization during active periods (kernel descheduling workers)
  for (const w of workerIds) {
    const actives = spans.workerSpans[w].actives;
    if (actives.length > 10) {
      const lowRatio = actives.filter(a => a.ratio < 0.5 && (a.end - a.start) > 1e6);
      if (lowRatio.length > actives.length * 0.1) {
        const avgRatio = lowRatio.reduce((s, a) => s + a.ratio, 0) / lowRatio.length;
        findings.push({
          severity: 'warning',
          check: 'cpu-contention',
          message: `Worker ${w}: ${lowRatio.length}/${actives.length} active periods have CPU ratio < 0.5 (avg ${avgRatio.toFixed(2)}) — kernel is descheduling this worker`,
        });
      }
    }
  }

  // 8. Kernel scheduling wait on unpark
  for (const w of workerIds) {
    const highSchedWait = spans.workerSpans[w].parks.filter(p => p.schedWait > 1e6); // >1ms in ns
    if (highSchedWait.length > 0) {
      const worst = highSchedWait.reduce((m, p) => Math.max(m, p.schedWait), -Infinity);
      findings.push({
        severity: worst > 10e6 ? 'warning' : 'info',
        check: 'kernel-sched-wait',
        message: `Worker ${w}: ${highSchedWait.length} unparks with kernel sched wait > 1ms (worst: ${(worst / 1e6).toFixed(1)}ms)`,
      });
    }
  }

  // 9. Many spans per poll (tight loop without yielding)
  if (trace.customEvents && trace.customEvents.length > 0) {
    const spanResult = buildSpanData(trace.customEvents);
    const { spansByWorker: sWorker } = spanResult;

    // 9. Many spans per poll (tight loop without yielding)
    for (const w of workerIds) {
      const wSpans = sWorker[w] || [];
      for (const p of spans.workerSpans[w].polls) {
        let lo = 0, hi = wSpans.length - 1;
        while (lo <= hi) { const mid = (lo + hi) >> 1; if (wSpans[mid].end < p.start) lo = mid + 1; else hi = mid - 1; }
        let count = 0;
        for (let i = lo; i < wSpans.length && wSpans[i].start <= p.end; i++) {
          if (wSpans[i].start >= p.start && wSpans[i].end <= p.end) count++;
        }
        if (count > 20) {
          findings.push({ severity: 'warning', check: 'many-spans-per-poll', message: `Worker ${w}: poll with ${count} spans (${((p.end - p.start) / 1e6).toFixed(2)}ms)` });
        }
      }
    }

    // 10. Span duration outliers (>10x the P50 for that span name)
    const allSpans = Object.values(sWorker).flat();
    const byName = {};
    for (const s of allSpans) (byName[s.spanName] ??= []).push(s.end - s.start);
    for (const [name, durations] of Object.entries(byName)) {
      if (durations.length < 10) continue;
      durations.sort((a, b) => a - b);
      const p50 = durations[Math.floor(durations.length * 0.5)];
      const threshold = p50 * 10;
      const outliers = durations.filter(d => d > threshold).length;
      if (outliers > 0) {
        findings.push({ severity: 'info', check: 'span-duration-outlier', message: `${name}: ${outliers} spans >10x P50 (P50=${(p50 / 1e3).toFixed(1)}µs, threshold=${(threshold / 1e3).toFixed(1)}µs)` });
      }
    }

    // 11. Unmatched span enters (cancelled tasks or segment boundary)
    if (spanResult.unmatchedSpans && spanResult.unmatchedSpans.length > 0) {
      const unmatched = spanResult.unmatchedSpans;
      const byName = {};
      for (const s of unmatched) (byName[s.spanName] ??= []).push(s);
      const summary = Object.entries(byName).map(([n, arr]) => `${n}(${arr.length})`).join(', ');
      findings.push({ severity: 'info', check: 'unmatched-spans', message: `${unmatched.length} spans with enter but no exit: ${summary}` });
    }
  }

  // Print findings
  console.log(`\n=== Red Flag Scan: ${tracePath} ===`);
  console.log(`Duration: ${durationMs.toFixed(1)}ms, ${workerIds.length} workers, ${trace.events.length} events\n`);

  if (findings.length === 0) {
    console.log('✅ No red flags found');
  } else {
    const icons = { critical: '🔴', warning: '🟡', info: 'ℹ️' };
    const sorted = findings.sort((a, b) => {
      const order = { critical: 0, warning: 1, info: 2 };
      return order[a.severity] - order[b.severity];
    });
    for (const f of sorted) {
      console.log(`${icons[f.severity]} [${f.check}] ${f.message}`);
    }
    console.log(`\n${findings.filter(f => f.severity === 'critical').length} critical, ${findings.filter(f => f.severity === 'warning').length} warnings, ${findings.filter(f => f.severity === 'info').length} info`);
  }
}

redFlagScan(process.argv[2] || 'trace.bin');
```

## Individual checks explained

### long-poll
A single `.poll()` call took too long. This blocks the worker thread from processing other tasks. Common causes: synchronous I/O, CPU-heavy computation, blocking mutex. Look at `poll.cpuSamples` and `poll.schedSamples` for stack traces showing what happened during the poll.

### task-leak
Active task count grows without bound. Tasks are being spawned but never completing. Check `taskSpawnLocs` for the spawn locations of unterminated tasks.

### sched-delay
Time between a task being woken (via `Waker::wake()`) and actually being polled. High delays mean all workers are busy — the woken task has to wait in the queue. Often caused by long polls or too many tasks for the worker count.

### blocking-calls
Scheduling samples (source=1) capture stack traces when the OS deschedules a worker. These reveal blocking system calls (file I/O, DNS resolution, mutex contention) happening on the async runtime. These should be moved to `spawn_blocking` or a dedicated thread.

### queue-depth
The global injection queue is where tasks go when no worker's local queue is available. High depth means the runtime can't keep up with incoming work.

### worker-imbalance
Large differences in poll counts between workers suggest work-stealing isn't distributing evenly, or one worker is stuck on long polls.

### cpu-contention
Workers are active (not parked) but spending less than 50% of wall time on CPU. The kernel is descheduling them — likely due to CPU contention from other processes or too many runtime threads for available cores.

### kernel-sched-wait
When a worker is woken (unparked), the kernel scheduling wait measures how long until the thread actually runs. High values indicate CPU contention at the OS level.

### many-spans-per-poll
A single poll contains many span enter/exit pairs. This usually means a tight loop of operations inside one poll without yielding. Cross-reference with long polls to find the culprit.

### span-duration-outlier
A span whose duration exceeds 10x the P50 for its name. Flags individual slow operations that may indicate contention, cold caches, or upstream latency spikes.

### unmatched-spans
Spans with an enter event but no matching exit. A small count is normal at trace segment boundaries (the span was open when the segment rotated). A large count may indicate task cancellation (the task was dropped before the span exited) or a bug in span instrumentation. The finding lists which span names are affected.
