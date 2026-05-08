#!/usr/bin/env node
"use strict";

const fs = require("fs");
const path = require("path");
const { parseTrace, EVENT_TYPES } = require("./trace_parser.js");

async function main() {
  const tracePath = process.argv[2] || path.join(__dirname, "demo-trace.bin");

  if (!fs.existsSync(tracePath)) {
    console.error(`Trace file not found: ${tracePath}`);
    process.exit(1);
  }

  function fail(msg) {
    console.log(`✗ ${msg}`);
    process.exit(1);
  }

  function pass(msg) {
    console.log(`✓ ${msg}`);
  }

  const buf = fs.readFileSync(tracePath);

  // ── Full parse (baseline) ──
  const full = await parseTrace(buf);
  console.log(`Full parse: ${full.events.length} events`);
  if (full.events.length === 0) fail("No events in full parse");
  if (full.truncated) fail("Full parse should not be truncated");
  if (full.timeFiltered) fail("Full parse should not be time-filtered");
  pass("Full parse: no truncation, no time filter");

  // ── Time range filtering ──
  const minTs = full.minTs;
  const maxTs = full.maxTs;
  const midTs = Math.floor((minTs + maxTs) / 2);

  // Filter first half
  const firstHalf = await parseTrace(buf, { startTime: minTs, endTime: midTs });
  if (firstHalf.events.length === 0) fail("First half has no events");
  if (firstHalf.events.length >= full.events.length) fail("First half should have fewer events than full");
  if (!firstHalf.timeFiltered) fail("First half should be time-filtered");
  if (firstHalf.filterStartTime !== minTs) fail("filterStartTime mismatch");
  if (firstHalf.filterEndTime !== midTs) fail("filterEndTime mismatch");
  for (const e of firstHalf.events) {
    if (e.timestamp < minTs || e.timestamp > midTs) {
      fail(`Event at ${e.timestamp} outside range [${minTs}, ${midTs}]`);
    }
  }
  pass(`First half: ${firstHalf.events.length} events, all in range`);

  // Filter second half
  const secondHalf = await parseTrace(buf, { startTime: midTs, endTime: maxTs });
  if (secondHalf.events.length === 0) fail("Second half has no events");
  for (const e of secondHalf.events) {
    if (e.timestamp < midTs || e.timestamp > maxTs) {
      fail(`Event at ${e.timestamp} outside range [${midTs}, ${maxTs}]`);
    }
  }
  pass(`Second half: ${secondHalf.events.length} events, all in range`);

  // Halves should cover most events (boundary events may be missed due to integer midpoint)
  const totalHalves = firstHalf.events.length + secondHalf.events.length;
  const missed = full.events.length - totalHalves;
  // With inclusive ranges and integer midpoint, at most a few events at exact boundary could be in both or neither
  if (missed > 0 && missed > 10) fail(`Halves missed ${missed} events (too many)`);
  pass(`Halves cover events (${totalHalves} vs ${full.events.length}, missed=${missed})`);

  // ── Narrow range ──
  const narrowStart = minTs + Math.floor((maxTs - minTs) * 0.4);
  const narrowEnd = minTs + Math.floor((maxTs - minTs) * 0.6);
  const narrow = await parseTrace(buf, { startTime: narrowStart, endTime: narrowEnd });
  if (narrow.events.length === 0) fail("Narrow range has no events");
  if (narrow.events.length >= full.events.length) fail("Narrow range should have fewer events");
  for (const e of narrow.events) {
    if (e.timestamp < narrowStart || e.timestamp > narrowEnd) {
      fail(`Narrow event at ${e.timestamp} outside range`);
    }
  }
  pass(`Narrow range (20% window): ${narrow.events.length} events`);

  // ── Symbol tables preserved across time filters ──
  // Symbol tables are uncapped frames and should always be present
  if (full.callframeSymbols.size > 0) {
    if (firstHalf.callframeSymbols.size !== full.callframeSymbols.size) {
      fail(`Symbol table size mismatch: filtered=${firstHalf.callframeSymbols.size} full=${full.callframeSymbols.size}`);
    }
    pass("Symbol tables preserved in time-filtered parse");
  } else {
    pass("No symbol tables to check (trace has none)");
  }

  // ── Task spawn/terminate preserved ──
  if (full.taskSpawnTimes.size > 0) {
    if (firstHalf.taskSpawnTimes.size + secondHalf.taskSpawnTimes.size < full.taskSpawnTimes.size) {
      // This is expected — task spawns outside the range are still tracked
    }
    // Task spawn times should be preserved regardless of time filter
    if (narrow.taskSpawnTimes.size !== full.taskSpawnTimes.size) {
      fail("Task spawn times should be preserved (uncapped frame)");
    }
    pass("Task lifecycle data preserved in time-filtered parse");
  }

  // ── maxEvents still works ──
  const limited = await parseTrace(buf, { maxEvents: 100 });
  if (limited.events.length > 100) fail(`maxEvents=100 but got ${limited.events.length}`);
  if (!limited.truncated) fail("Should be truncated with maxEvents=100");
  pass(`maxEvents=100: ${limited.events.length} events, truncated=true`);

  // ── Combined maxEvents + time range ──
  const combined = await parseTrace(buf, { maxEvents: 50, startTime: minTs, endTime: midTs });
  if (combined.events.length > 50) fail(`Combined: got ${combined.events.length} > 50`);
  for (const e of combined.events) {
    if (e.timestamp < minTs || e.timestamp > midTs) {
      fail(`Combined event outside time range`);
    }
  }
  pass(`Combined maxEvents+timeRange: ${combined.events.length} events`);

  // ── Empty range ──
  const empty = await parseTrace(buf, { startTime: maxTs + 1e9, endTime: maxTs + 2e9 });
  if (empty.events.length !== 0) fail(`Empty range should have 0 events, got ${empty.events.length}`);
  pass("Empty range: 0 events");

  console.log("\n✓ All time range filtering tests passed!");
}

main().catch((e) => {
  console.error(e);
  process.exit(1);
});
