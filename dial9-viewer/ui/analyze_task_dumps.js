#!/usr/bin/env node
// Analyze task dumps in a trace file
const fs = require("fs");
const path = require("path");
const parser = require(path.resolve(__dirname, "trace_parser.js"));
const analysis = require(path.resolve(__dirname, "trace_analysis.js"));

const traceFile = process.argv[2] || path.resolve(__dirname, "demo-trace.bin");
const buf = fs.readFileSync(traceFile);

(async () => {
  const trace = await parser.parseTrace(buf);
  const dumps = trace.taskDumps || [];
  console.log(`\n=== Task Dump Analysis ===`);
  console.log(`Total task dumps: ${dumps.length}`);

  if (!dumps.length) {
    console.log("No task dumps found!");
    process.exit(1);
  }

  // Unique tasks
  const taskIds = new Set(dumps.map((d) => d.task_id));
  console.log(`Unique tasks with dumps: ${taskIds.size}`);

  // Frame stats
  const frameCounts = dumps.map((d) => d.callchain.length);
  const avg = (frameCounts.reduce((a, b) => a + b, 0) / frameCounts.length).toFixed(1);
  const min = Math.min(...frameCounts);
  const max = Math.max(...frameCounts);
  console.log(`Frames per dump: avg=${avg}, min=${min}, max=${max}`);

  // Symbolize a sample
  const symbols = trace.callframeSymbols;
  console.log(`\nSymbol table entries: ${symbols.size}`);

  // Show first 3 dumps with symbolized frames
  console.log(`\n--- Sample task dumps (first 3) ---`);
  for (let i = 0; i < Math.min(3, dumps.length); i++) {
    const d = dumps[i];
    const tsMs = (d.timestamp / 1e6).toFixed(2);
    console.log(`\nDump ${i}: taskId=0x${d.task_id.toString(16)} ts=${tsMs}ms frames=${d.callchain.length}`);
    for (let j = 0; j < Math.min(20, d.callchain.length); j++) {
      const addr = d.callchain[j];
      const sym = symbols.get(addr);
      if (sym) {
        if (Array.isArray(sym)) {
          for (const s of sym) {
            const loc = s.location ? ` (${s.location})` : "";
            console.log(`  [${j}] ${s.symbol}${loc}`);
          }
        } else {
          const loc = sym.location ? ` (${sym.location})` : "";
          console.log(`  [${j}] ${sym.symbol}${loc}`);
        }
      } else {
        console.log(`  [${j}] ${addr}`);
      }
    }
    if (d.callchain.length > 20) {
      console.log(`  ... ${d.callchain.length - 20} more frames`);
    }
  }

  // Analyze frame quality: how many frames are runtime internals?
  console.log(`\n--- Frame quality analysis ---`);
  const runtimePatterns = [
    "tokio::", "std::future", "core::future", "<core::pin",
    "futures_util::", "poll_fn", "__rust_begin_short_backtrace",
    "backtrace::", "dial9_tokio_telemetry::traced",
  ];
  let totalFrames = 0;
  let runtimeFrames = 0;
  let userFrames = 0;
  let unsymbolized = 0;

  for (const d of dumps) {
    for (const addr of d.callchain) {
      totalFrames++;
      const sym = symbols.get(addr);
      if (!sym) { unsymbolized++; continue; }
      const name = Array.isArray(sym) ? sym[0].symbol : sym.symbol;
      if (runtimePatterns.some((p) => name.includes(p))) {
        runtimeFrames++;
      } else {
        userFrames++;
      }
    }
  }
  console.log(`Total frames: ${totalFrames}`);
  console.log(`Runtime/internal: ${runtimeFrames} (${((runtimeFrames / totalFrames) * 100).toFixed(1)}%)`);
  console.log(`User code: ${userFrames} (${((userFrames / totalFrames) * 100).toFixed(1)}%)`);
  console.log(`Unsymbolized: ${unsymbolized} (${((unsymbolized / totalFrames) * 100).toFixed(1)}%)`);

  // Attach to idle periods
  const workerIds = [...new Set(trace.events.map((e) => e.workerId))].sort((a, b) => a - b);
  const { workerSpans } = analysis.buildWorkerSpans(trace.events, workerIds, Infinity);
  const idlesByTask = analysis.attachTaskDumps(trace, workerSpans);
  let idlesWithDumps = 0;
  let totalIdles = 0;
  for (const idles of idlesByTask.values()) {
    for (const idle of idles) {
      totalIdles++;
      if (idle.dumps.length > 0) idlesWithDumps++;
    }
  }
  console.log(`\n--- Idle period attachment ---`);
  console.log(`Total idle periods: ${totalIdles}`);
  console.log(`Idle periods with dumps: ${idlesWithDumps}`);

  console.log(`\n✓ Analysis complete`);
})();
