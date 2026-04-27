#!/usr/bin/env node
"use strict";
// Scale test for directory parsing.
//
// Usage:
//   node test_directory_scale.js              # default: 20 files
//   node test_directory_scale.js --large      # 200 files

const fs = require("fs");
const path = require("path");
const os = require("os");
const { parseTrace, EVENT_TYPES } = require("./trace_parser.js");
const {
  buildWorkerSpans,
  attachCpuSamples,
  computeSchedulingDelays,
} = require("./trace_analysis.js");

const LARGE = process.argv.includes("--large");
const FILE_COUNT = LARGE ? 200 : 20;

let failures = 0;
function fail(msg) { console.log(`✗ ${msg}`); failures++; }
function pass(msg) { console.log(`✓ ${msg}`); }
function assert(cond, msg) { if (cond) pass(msg); else fail(msg); }

function setupDir(n) {
  const dir = fs.mkdtempSync(path.join(os.tmpdir(), "d9-scale-"));
  const demo = path.join(__dirname, "demo-trace.bin");
  for (let i = 0; i < n; i++) {
    fs.copyFileSync(demo, path.join(dir, `seg-${String(i).padStart(4, "0")}.bin`));
  }
  return dir;
}
function cleanup(dir) { fs.rmSync(dir, { recursive: true, force: true }); }

async function main() {
  if (!fs.existsSync(path.join(__dirname, "demo-trace.bin"))) {
    console.error("demo-trace.bin not found"); process.exit(1);
  }

  console.log(`Scale test: ${FILE_COUNT} files (${LARGE ? "--large" : "default"})\n`);
  const dir = setupDir(FILE_COUNT);

  try {
    // ── Cold parse ──
    console.log("Cold parse:");
    const t0 = Date.now();
    let coldCount = 0;
    for await (const trace of parseTrace(dir)) {
      coldCount++;
      if (coldCount === 1) {
        assert(trace.events.length > 0, "first file has events");
        assert(trace.taskSpawnTimes instanceof Map, "Maps intact");
      }
    }
    const coldMs = Date.now() - t0;
    assert(coldCount === FILE_COUNT, `cold: iterated all ${FILE_COUNT} files`);
    console.log(`  Cold: ${coldMs}ms (${(coldMs / FILE_COUNT).toFixed(0)}ms/file)\n`);

    // ── Warm parse ──
    console.log("Warm parse:");
    const t1 = Date.now();
    let warmCount = 0;
    for await (const trace of parseTrace(dir)) {
      warmCount++;
      if (warmCount === 1) assert(trace.events.length > 0, "cache hit: has events");
    }
    const warmMs = Date.now() - t1;
    assert(warmCount === FILE_COUNT, `warm: iterated all ${FILE_COUNT} files`);
    assert(warmMs < coldMs, `warm faster than cold (${warmMs}ms < ${coldMs}ms)`);
    console.log(`  Warm: ${warmMs}ms (${(warmMs / FILE_COUNT).toFixed(0)}ms/file)\n`);

    // ── Sampled parse ──
    console.log("Sampled parse:");
    const sampleN = Math.min(10, FILE_COUNT);
    const t2 = Date.now();
    let sampleCount = 0;
    for await (const _ of parseTrace(dir, { sample: sampleN })) { sampleCount++; }
    const sampleMs = Date.now() - t2;
    assert(sampleCount === sampleN, `sample: iterated ${sampleN} files`);
    console.log(`  Sampled (${sampleN}): ${sampleMs}ms\n`);

    // ── Full pipeline on cached data ──
    console.log("Full pipeline on cached data:");
    const t3 = Date.now();
    let worstPollMs = 0;
    let totalDelays = 0;
    let pipelineCount = 0;

    for await (const trace of parseTrace(dir)) {
      const workerIds = [...new Set(
        trace.events
          .filter(e => e.eventType !== EVENT_TYPES.QueueSample && e.eventType !== EVENT_TYPES.WakeEvent)
          .map(e => e.workerId)
      )].sort((a, b) => a - b);
      const maxTs = trace.events.reduce((m, e) => Math.max(m, e.timestamp), -Infinity);
      const spans = buildWorkerSpans(trace.events, workerIds, maxTs);
      attachCpuSamples(trace.cpuSamples, spans.workerSpans);
      const schedDelays = computeSchedulingDelays(spans.workerSpans, workerIds, spans.wakesByTask);
      totalDelays += schedDelays.length;

      for (const w of workerIds) {
        for (const p of spans.workerSpans[w].polls) {
          const dur = (p.end - p.start) / 1e6;
          if (dur > worstPollMs) worstPollMs = dur;
        }
      }
      pipelineCount++;
    }
    const pipelineMs = Date.now() - t3;
    assert(pipelineCount === FILE_COUNT, `pipeline: processed all ${FILE_COUNT} files`);
    assert(worstPollMs > 0, `pipeline: found worst poll (${worstPollMs.toFixed(1)}ms)`);
    assert(totalDelays > 0, `pipeline: found ${totalDelays} scheduling delays`);
    console.log(`  Pipeline: ${pipelineMs}ms (${(pipelineMs / FILE_COUNT).toFixed(0)}ms/file)\n`);

  } finally {
    cleanup(dir);
  }

  console.log(`${failures === 0 ? "✓" : "✗"} Scale test ${failures === 0 ? "passed" : `failed (${failures})`}!`);
  if (failures > 0) process.exit(1);
}

main().catch((err) => { console.error(err); process.exit(1); });
