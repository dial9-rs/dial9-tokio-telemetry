#!/usr/bin/env node
"use strict";

const fs = require("fs");
const path = require("path");
const os = require("os");
const { parseTrace, EVENT_TYPES } = require("./trace_parser.js");

let failures = 0;
function fail(msg) { console.log(`✗ ${msg}`); failures++; }
function pass(msg) { console.log(`✓ ${msg}`); }
function assert(cond, msg) { if (cond) pass(msg); else fail(msg); }

/** Collect first trace from the async iterable. */
async function first(input, opts) {
  for await (const t of parseTrace(input, opts)) return t;
  throw new Error("no traces");
}

/** Collect all traces from the async iterable. */
async function collect(input, opts) {
  const all = [];
  for await (const t of parseTrace(input, opts)) all.push(t);
  return all;
}

function setupDir(n) {
  const dir = fs.mkdtempSync(path.join(os.tmpdir(), "d9-test-dir-"));
  const demo = path.join(__dirname, "demo-trace.bin");
  for (let i = 0; i < n; i++) {
    fs.copyFileSync(demo, path.join(dir, `trace-${String(i).padStart(3, "0")}.bin`));
  }
  return dir;
}
function cleanup(dir) { fs.rmSync(dir, { recursive: true, force: true }); }

async function main() {
  const demoPath = path.join(__dirname, "demo-trace.bin");
  if (!fs.existsSync(demoPath)) { console.error("demo-trace.bin not found"); process.exit(1); }

  // ── Single file: async iterable yields one ParsedTrace ──
  console.log("\nparseTrace with file path:");
  {
    const trace = await first(demoPath);
    assert(trace.magic === "D9TF", "file: valid trace");
    assert(trace.events.length > 0, "file: has events");
    assert(trace.taskSpawnTimes instanceof Map, "file: taskSpawnTimes is Map");
    assert(trace.callframeSymbols instanceof Map, "file: callframeSymbols is Map");
    assert(trace.cpuSamples.length > 0, "file: has cpuSamples");
  }

  // ── Buffer: returns Promise<ParsedTrace> (backwards compatible) ──
  console.log("\nparseTrace with buffer:");
  {
    const buf = fs.readFileSync(demoPath);
    const trace = await parseTrace(buf);
    assert(trace.magic === "D9TF", "buffer: valid trace");
    assert(trace.events.length > 0, "buffer: has events");
  }

  // ── Directory: yields one ParsedTrace per file ──
  console.log("\nparseTrace with directory:");
  {
    const dir = setupDir(3);
    try {
      const traces = await collect(dir);
      assert(traces.length === 3, `dir: 3 traces (got ${traces.length})`);
      for (const trace of traces) {
        assert(trace.magic === "D9TF", "dir: valid trace");
        assert(trace.events.length > 0, "dir: has events");
        assert(trace.taskSpawnTimes instanceof Map, "dir: taskSpawnTimes is Map");
        assert(trace.callframeSymbols instanceof Map, "dir: callframeSymbols is Map");
        assert(trace.cpuSamples.length > 0, "dir: has cpuSamples");
      }
    } finally {
      cleanup(dir);
    }
  }

  // ── Same shape for file and directory ──
  console.log("\nUnified shape:");
  {
    const dir = setupDir(1);
    try {
      const fromFile = await first(demoPath);
      const fromDir = await first(dir);
      const keys = ["magic", "events", "cpuSamples", "taskSpawnTimes", "callframeSymbols", "customEvents"];
      for (const k of keys) {
        assert(k in fromFile && k in fromDir, `unified: both have '${k}'`);
      }
    } finally {
      cleanup(dir);
    }
  }

  // ── Caching ──
  console.log("\nCaching:");
  {
    const dir = setupDir(2);
    try {
      await collect(dir);
      const cacheDir = path.join(dir, ".d9-cache");
      assert(fs.existsSync(cacheDir), "cache: .d9-cache created");
      const cached = fs.readdirSync(cacheDir).filter(f => f.endsWith(".json"));
      assert(cached.length === 2, `cache: 2 files (got ${cached.length})`);

      // Warm run
      const warm = await collect(dir);
      assert(warm.length === 2, "cache hit: 2 traces");
      assert(warm[0].events.length > 0, "cache hit: has events");
      assert(warm[0].taskSpawnTimes instanceof Map, "cache hit: Maps reconstructed");
    } finally {
      cleanup(dir);
    }
  }

  // ── Cache invalidation ──
  console.log("\nCache invalidation:");
  {
    const dir = setupDir(1);
    try {
      await collect(dir);
      const cacheDir = path.join(dir, ".d9-cache");
      const cp = path.join(cacheDir, fs.readdirSync(cacheDir)[0]);
      const mtimeBefore = fs.statSync(cp).mtimeMs;
      await new Promise(r => setTimeout(r, 50));
      const src = path.join(dir, fs.readdirSync(dir).filter(f => f.endsWith(".bin"))[0]);
      fs.utimesSync(src, new Date(), new Date());
      await collect(dir);
      assert(fs.statSync(cp).mtimeMs > mtimeBefore, "invalidation: cache updated");
    } finally {
      cleanup(dir);
    }
  }

  // ── Force ──
  console.log("\nForce:");
  {
    const dir = setupDir(1);
    try {
      await collect(dir);
      const cacheDir = path.join(dir, ".d9-cache");
      const cp = path.join(cacheDir, fs.readdirSync(cacheDir)[0]);
      const mtimeBefore = fs.statSync(cp).mtimeMs;
      await new Promise(r => setTimeout(r, 50));
      await collect(dir, { force: true });
      assert(fs.statSync(cp).mtimeMs > mtimeBefore, "force: cache rewritten");
    } finally {
      cleanup(dir);
    }
  }

  // ── Sample ──
  console.log("\nSample:");
  {
    const dir = setupDir(10);
    try {
      const traces = await collect(dir, { sample: 3 });
      assert(traces.length === 3, `sample: 3 traces (got ${traces.length})`);
    } finally {
      cleanup(dir);
    }
  }

  // ── Sample validation ──
  console.log("\nSample validation:");
  {
    const dir = setupDir(3);
    try {
      let threw = false;
      try { await collect(dir, { sample: 0 }); }
      catch (e) { threw = true; assert(e.message.includes("sample"), `sample=0: ${e.message}`); }
      assert(threw, "sample=0: throws");
    } finally {
      cleanup(dir);
    }
  }

  // ── Cache disabled ──
  console.log("\nCache disabled:");
  {
    const dir = setupDir(2);
    try {
      const traces = await collect(dir, { cache: false });
      assert(traces.length === 2, "no-cache: 2 traces");
      assert(traces[0].events.length > 0, "no-cache: has events");
      assert(!fs.existsSync(path.join(dir, ".d9-cache")), "no-cache: no .d9-cache");
    } finally {
      cleanup(dir);
    }
  }

  // ── Empty directory ──
  console.log("\nEmpty directory:");
  {
    const dir = fs.mkdtempSync(path.join(os.tmpdir(), "d9-test-empty-"));
    try {
      let threw = false;
      try { await collect(dir); }
      catch (e) { threw = true; assert(e.message.includes("No .bin"), `empty: ${e.message}`); }
      assert(threw, "empty: throws");
    } finally {
      cleanup(dir);
    }
  }

  // ── Progress ──
  console.log("\nProgress:");
  {
    const dir = setupDir(3);
    try {
      const progress = [];
      await collect(dir, { onParseProgress: p => progress.push(p) });
      assert(progress.length >= 3, `progress: >= 3 calls (got ${progress.length})`);
    } finally {
      cleanup(dir);
    }
  }

  // ── Parallel=false ──
  console.log("\nParallel=false:");
  {
    const dir = setupDir(3);
    try {
      const traces = await collect(dir, { parallel: false });
      assert(traces.length === 3, "sequential: 3 traces");
      assert(traces[0].events.length > 0, "sequential: has events");
    } finally {
      cleanup(dir);
    }
  }

  // ── Atomic writes ──
  console.log("\nAtomic writes:");
  {
    const dir = setupDir(1);
    try {
      await collect(dir);
      const cacheDir = path.join(dir, ".d9-cache");
      const tmps = fs.readdirSync(cacheDir).filter(f => f.endsWith(".tmp"));
      assert(tmps.length === 0, "atomic: no .tmp files");
    } finally {
      cleanup(dir);
    }
  }

  // ── Full analysis pipeline works on each yielded trace ──
  console.log("\nAnalysis pipeline:");
  {
    const { buildWorkerSpans, attachCpuSamples, computeSchedulingDelays } = require("./trace_analysis.js");
    const dir = setupDir(2);
    try {
      for await (const trace of parseTrace(dir)) {
        const workerIds = [...new Set(
          trace.events.filter(e => e.eventType !== EVENT_TYPES.QueueSample && e.eventType !== EVENT_TYPES.WakeEvent)
            .map(e => e.workerId)
        )].sort((a, b) => a - b);
        assert(workerIds.length > 0, "pipeline: has workers");
        const maxTs = trace.events.reduce((m, e) => Math.max(m, e.timestamp), -Infinity);
        const spans = buildWorkerSpans(trace.events, workerIds, maxTs);
        assert(spans.workerSpans[workerIds[0]].polls.length > 0, "pipeline: has polls");
        const { pollsWithCpuSamples } = attachCpuSamples(trace.cpuSamples, spans.workerSpans);
        assert(pollsWithCpuSamples > 0, "pipeline: attachCpuSamples works");
        const schedDelays = computeSchedulingDelays(spans.workerSpans, workerIds, spans.wakesByTask);
        assert(schedDelays.length > 0, "pipeline: has schedDelays");
      }
    } finally {
      cleanup(dir);
    }
  }

  // ── analyzeTraces respects sample option ──
  console.log("\nanalyzeTraces sample:");
  {
    const { analyzeTraces } = require(path.resolve(__dirname, '..', 'skills', 'analyze.js'));
    const dir = setupDir(6);
    try {
      // Full run to populate cache for all 6 files
      const full = await analyzeTraces(dir);
      const fullEvents = full.eventCount;

      // Sampled run should only analyze 3 files, not all 6 cached
      const sampled = await analyzeTraces(dir, { sample: 3 });
      assert(sampled.eventCount > 0, "sample: has events");
      assert(sampled.eventCount < fullEvents, `sample: fewer events than full (${sampled.eventCount} < ${fullEvents})`);
      // Each file has the same event count, so sampled should be ~half
      const expectedRatio = 3 / 6;
      const actualRatio = sampled.eventCount / fullEvents;
      assert(Math.abs(actualRatio - expectedRatio) < 0.01, `sample: event ratio ~0.5 (got ${actualRatio.toFixed(3)})`);
    } finally {
      cleanup(dir);
    }
  }

  // ── analyzeTraces respects force option ──
  console.log("\nanalyzeTraces force:");
  {
    const { analyzeTraces } = require(path.resolve(__dirname, '..', 'skills', 'analyze.js'));
    const dir = setupDir(2);
    try {
      // First run populates cache
      await analyzeTraces(dir);
      const cacheDir = path.join(dir, '.d9-cache');
      const cacheFile = path.join(cacheDir, fs.readdirSync(cacheDir).filter(f => f.endsWith('.json'))[0]);
      const mtimeBefore = fs.statSync(cacheFile).mtimeMs;
      await new Promise(r => setTimeout(r, 50));

      // Force should re-parse even though cache is valid
      await analyzeTraces(dir, { force: true });
      const mtimeAfter = fs.statSync(cacheFile).mtimeMs;
      assert(mtimeAfter > mtimeBefore, "force: cache file was rewritten");
    } finally {
      cleanup(dir);
    }
  }

  console.log(`\n${failures === 0 ? "✓ All" : "✗ " + failures + " failed,"} directory parsing tests ${failures === 0 ? "passed" : ""}!`);
  if (failures > 0) process.exit(1);
}

main().catch(err => { console.error(err); process.exit(1); });
