#!/usr/bin/env node
// Red flag scan for dial9 traces — surfaces common Tokio runtime problems.
// Usage: node red_flag_scan.js <trace.bin or directory>
"use strict";

const fs = require('fs');
const path = require('path');

function resolve(name) {
  const sibling = path.resolve(__dirname, name);
  if (fs.existsSync(sibling)) return sibling;
  // When running from .symposium/skills/dial9-red-flags/scripts/, look for toolkit scripts
  const toolkit = path.resolve(__dirname, '..', '..', 'dial9-toolkit', 'scripts', name);
  if (fs.existsSync(toolkit)) return toolkit;
  return path.resolve(__dirname, '..', '..', '..', '..', 'ui', name);
}

const { parseTrace, EVENT_TYPES, deduplicateSamples } = require(resolve('trace_parser.js'));
const { buildWorkerSpans, attachCpuSamples, buildActiveTaskTimeline,
        computeSchedulingDelays, buildSpanData } = require(resolve('trace_analysis.js'));

async function redFlagScan(tracePath) {
  for await (const trace of parseTrace(tracePath)) {
    const workerIds = [...new Set(
      trace.events.filter(e => e.eventType !== EVENT_TYPES.QueueSample && e.eventType !== EVENT_TYPES.WakeEvent)
        .map(e => e.workerId)
    )].sort((a, b) => a - b);
    const maxTs = trace.maxTs;
    const minTs = trace.minTs;
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

    // 3. High scheduling delays
    const highDelays = schedDelays.filter(d => d.delay > 5e6);
    if (highDelays.length > 0) {
      const worst = highDelays.reduce((m, d) => Math.max(m, d.delay), -Infinity);
      findings.push({
        severity: worst > 50e6 ? 'critical' : 'warning',
        check: 'sched-delay',
        message: `${highDelays.length} scheduling delays > 5ms (worst: ${(worst / 1e6).toFixed(1)}ms) — tasks waiting for busy workers`,
      });
    }

    // 4. Blocking calls via scheduling samples
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

    // 7. Low CPU utilization during active periods
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
      const highSchedWait = spans.workerSpans[w].parks.filter(p => p.schedWait > 1e6);
      if (highSchedWait.length > 0) {
        const worst = highSchedWait.reduce((m, p) => Math.max(m, p.schedWait), -Infinity);
        findings.push({
          severity: worst > 10e6 ? 'warning' : 'info',
          check: 'kernel-sched-wait',
          message: `Worker ${w}: ${highSchedWait.length} unparks with kernel sched wait > 1ms (worst: ${(worst / 1e6).toFixed(1)}ms)`,
        });
      }
    }

    // Span-based checks (require tracing layer)
    if (trace.customEvents && trace.customEvents.length > 0) {
      const spanResult = buildSpanData(trace.customEvents);
      const { spansByWorker: sWorker } = spanResult;

      // 9. Many spans per poll
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

      // 10. Span duration outliers
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

      // 11. Unmatched span enters
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
}

if (require.main === module) {
  redFlagScan(process.argv[2] || 'trace.bin').catch(err => { console.error(err); process.exit(1); });
}

module.exports = { redFlagScan };
