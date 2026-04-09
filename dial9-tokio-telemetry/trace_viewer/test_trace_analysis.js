#!/usr/bin/env node
"use strict";

const fs = require("fs");
const path = require("path");
const { EVENT_TYPES, parseTrace } = require("./trace_parser.js");
const {
  buildWorkerSpans,
  attachCpuSamples,
  extractLocalQueueSamples,
  buildActiveTaskTimeline,
  indexWakeEvents,
  computeSchedulingDelays,
  filterPointsOfInterest,
  buildFlamegraphTree,
  flattenFlamegraph,
  buildFgData,
} = require("./trace_analysis.js");

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

  const trace = await parseTrace(fs.readFileSync(tracePath));
  const evts = trace.events;

  const wSet = new Set();
  evts.forEach((e) => {
    if (
      e.eventType !== EVENT_TYPES.QueueSample &&
      e.eventType !== EVENT_TYPES.WakeEvent
    )
      wSet.add(e.workerId);
  });
  const workerIds = [...wSet].sort((a, b) => a - b);

  let minTs = evts[0].timestamp;
  let maxTs = evts[evts.length - 1].timestamp;
  for (const e of evts) {
    if (e.timestamp < minTs) minTs = e.timestamp;
    if (e.timestamp > maxTs) maxTs = e.timestamp;
  }

  // ── buildWorkerSpans ──

  const { workerSpans, perWorker, queueSamples } = buildWorkerSpans(
    evts,
    workerIds,
    maxTs
  );

  function testPollsHaveValidRange() {
    for (const w of workerIds) {
      for (const p of workerSpans[w].polls) {
        if (p.start > p.end)
          fail(`Worker ${w}: poll start > end (${p.start} > ${p.end})`);
      }
    }
    pass("All polls have start <= end");
  }

  function testNoOverlappingPolls() {
    for (const w of workerIds) {
      const polls = workerSpans[w].polls;
      for (let i = 1; i < polls.length; i++) {
        if (polls[i].start < polls[i - 1].end)
          fail(`Worker ${w}: overlapping polls at index ${i}`);
      }
    }
    pass("No overlapping polls on same worker");
  }

  function testActiveRatiosInRange() {
    for (const w of workerIds) {
      for (const a of workerSpans[w].actives) {
        if (a.ratio < 0 || a.ratio > 1)
          fail(`Worker ${w}: active ratio ${a.ratio} out of [0, 1]`);
      }
    }
    pass("Active period ratios in [0, 1]");
  }

  function testParksHaveValidRange() {
    for (const w of workerIds) {
      for (const p of workerSpans[w].parks) {
        if (p.start > p.end) fail(`Worker ${w}: park start > end`);
      }
    }
    pass("All parks have start <= end");
  }

  function testQueueSamplesExist() {
    if (queueSamples.length === 0) fail("No queue samples");
    pass(`${queueSamples.length} queue samples`);
  }

  // ── attachCpuSamples ──

  const cpuResult = attachCpuSamples(trace.cpuSamples, workerSpans);

  function testAttachedSamplesWithinPollBounds() {
    for (const w of workerIds) {
      for (const p of workerSpans[w].polls) {
        if (p.cpuSamples) {
          for (const s of p.cpuSamples) {
            if (s.timestamp < p.start || s.timestamp > p.end)
              fail(
                `Worker ${w}: cpu sample at ${s.timestamp} outside poll [${p.start}, ${p.end}]`
              );
          }
        }
        if (p.schedSamples) {
          for (const s of p.schedSamples) {
            if (s.timestamp < p.start || s.timestamp > p.end)
              fail(
                `Worker ${w}: sched sample at ${s.timestamp} outside poll [${p.start}, ${p.end}]`
              );
          }
        }
      }
    }
    pass("All attached samples fall within poll bounds");
  }

  function testCpuResultCounts() {
    if (
      cpuResult.pollsWithCpuSamples < 0 ||
      cpuResult.pollsWithSchedSamples < 0
    )
      fail("Negative sample counts");
    pass(
      `${cpuResult.pollsWithCpuSamples} polls with cpu samples, ${cpuResult.pollsWithSchedSamples} with sched samples`
    );
  }

  // ── extractLocalQueueSamples ──

  const { workerQueueSamples, maxLocalQueue } = extractLocalQueueSamples(
    perWorker,
    workerIds
  );

  function testLocalQueueNonNegative() {
    for (const w of workerIds) {
      for (const s of workerQueueSamples[w]) {
        if (s.local < 0) fail(`Worker ${w}: negative local queue ${s.local}`);
      }
    }
    pass("All local queue depths non-negative");
  }

  function testMaxLocalQueue() {
    if (maxLocalQueue < 1) fail(`maxLocalQueue ${maxLocalQueue} < 1`);
    pass(`maxLocalQueue = ${maxLocalQueue}`);
  }

  // ── buildActiveTaskTimeline ──

  const { activeTaskSamples, taskFirstPoll } = buildActiveTaskTimeline(
    trace.taskSpawnTimes,
    trace.taskTerminateTimes
  );

  function testTimelineSorted() {
    for (let i = 1; i < activeTaskSamples.length; i++) {
      if (activeTaskSamples[i].t < activeTaskSamples[i - 1].t)
        fail(`Timeline not sorted at index ${i}`);
    }
    pass("Timeline sorted by timestamp");
  }

  function testCountNonNegative() {
    for (const s of activeTaskSamples) {
      if (s.count < 0) fail(`Negative task count ${s.count}`);
    }
    pass("Task counts non-negative");
  }

  // ── indexWakeEvents ──

  const { wakesByTask, wakesByWorker } = indexWakeEvents(evts);

  function testWakesByTaskSorted() {
    for (const arr of Object.values(wakesByTask)) {
      for (let i = 1; i < arr.length; i++) {
        if (arr[i].timestamp < arr[i - 1].timestamp)
          fail("wakesByTask not sorted");
      }
    }
    pass("wakesByTask arrays sorted by timestamp");
  }

  function testWakesByWorkerSorted() {
    for (const arr of Object.values(wakesByWorker)) {
      for (let i = 1; i < arr.length; i++) {
        if (arr[i].timestamp < arr[i - 1].timestamp)
          fail("wakesByWorker not sorted");
      }
    }
    pass("wakesByWorker arrays sorted by timestamp");
  }

  function testWakeCountsConsistent() {
    let taskTotal = 0;
    for (const arr of Object.values(wakesByTask)) taskTotal += arr.length;
    let workerTotal = 0;
    for (const arr of Object.values(wakesByWorker)) workerTotal += arr.length;
    if (taskTotal !== workerTotal)
      fail(
        `wakesByTask total ${taskTotal} != wakesByWorker total ${workerTotal}`
      );
    pass(`${taskTotal} wake events indexed consistently`);
  }

  // ── computeSchedulingDelays ──

  const schedDelays = computeSchedulingDelays(
    workerSpans,
    workerIds,
    wakesByTask
  );

  function testDelaysPositive() {
    for (const sd of schedDelays) {
      if (sd.delay <= 0) fail(`Non-positive delay: ${sd.delay}`);
    }
    pass("All delays positive");
  }

  function testDelaysBounded() {
    for (const sd of schedDelays) {
      if (sd.delay >= 1e9) fail(`Delay >= 1s: ${sd.delay}`);
    }
    pass("All delays < 1s");
  }

  function testWakeBeforePoll() {
    for (const sd of schedDelays) {
      if (sd.wakeTime >= sd.pollTime)
        fail(`wakeTime ${sd.wakeTime} >= pollTime ${sd.pollTime}`);
    }
    pass("wakeTime < pollTime for all delays");
  }

  function testDelaysSorted() {
    for (let i = 1; i < schedDelays.length; i++) {
      if (schedDelays[i].wakeTime < schedDelays[i - 1].wakeTime)
        fail("schedDelays not sorted by wakeTime");
    }
    pass("schedDelays sorted by wakeTime");
  }

  // ── filterPointsOfInterest ──

  function testLongPollFilter() {
    const pois = filterPointsOfInterest(
      "long-poll",
      workerSpans,
      workerIds,
      schedDelays,
      trace.hasSchedWait,
      {}
    );
    for (const p of pois) {
      if (p.type !== "long-poll") fail(`Wrong type: ${p.type}`);
      if (p.value <= 1) fail(`long-poll value ${p.value} <= 1ms`);
    }
    pass(`long-poll filter: ${pois.length} results, all > 1ms`);
  }

  function testCpuSampledFilter() {
    const pois = filterPointsOfInterest(
      "cpu-sampled",
      workerSpans,
      workerIds,
      schedDelays,
      trace.hasSchedWait,
      {}
    );
    for (const p of pois) {
      if (p.type !== "cpu-sampled") fail(`Wrong type: ${p.type}`);
      if (p.value <= 0) fail(`cpu-sampled value ${p.value} <= 0`);
    }
    pass(`cpu-sampled filter: ${pois.length} results, all with samples`);
  }

  function testWakeDelayFilter() {
    const pois = filterPointsOfInterest(
      "wake-delay",
      workerSpans,
      workerIds,
      schedDelays,
      trace.hasSchedWait,
      {}
    );
    for (const p of pois) {
      if (p.type !== "wake-delay") fail(`Wrong type: ${p.type}`);
      if (p.value <= 100) fail(`wake-delay value ${p.value} <= 100µs`);
    }
    pass(`wake-delay filter: ${pois.length} results, all > 100µs`);
  }

  function testSortByWorst() {
    const pois = filterPointsOfInterest(
      "long-poll",
      workerSpans,
      workerIds,
      schedDelays,
      trace.hasSchedWait,
      { sortByWorst: true }
    );
    for (let i = 1; i < pois.length; i++) {
      if (pois[i].value > pois[i - 1].value) fail("sortByWorst not descending");
    }
    pass("sortByWorst produces descending order");
  }

  // ── buildFlamegraphTree / flattenFlamegraph ──

  function testFlamegraphTree() {
    const cpuSamples = trace.cpuSamples.filter((s) => s.source !== 1);
    if (cpuSamples.length === 0)
      return pass("No CPU samples to test flamegraph");

    const root = buildFlamegraphTree(cpuSamples, trace.callframeSymbols);
    if (root.count !== cpuSamples.length)
      fail(`Root count ${root.count} != sample count ${cpuSamples.length}`);
    pass(`Root count matches sample count (${root.count})`);
  }

  function testFlattenFlamegraph() {
    const cpuSamples = trace.cpuSamples.filter((s) => s.source !== 1);
    if (cpuSamples.length === 0) return pass("No CPU samples to test flatten");

    const root = buildFlamegraphTree(cpuSamples, trace.callframeSymbols);
    const { nodes, maxDepth } = flattenFlamegraph(root, cpuSamples.length);
    for (const n of nodes) {
      if (n.x < 0 || n.x >= 1) fail(`Node x=${n.x} out of [0, 1)`);
      if (n.w <= 0) fail(`Node w=${n.w} <= 0`);
    }
    if (maxDepth < 0) fail(`maxDepth ${maxDepth} < 0`);
    pass(`${nodes.length} flamegraph nodes, maxDepth=${maxDepth}`);
  }

  function testBuildFgData() {
    const cpuSamples = trace.cpuSamples.filter((s) => s.source !== 1);
    if (cpuSamples.length === 0)
      return pass("No CPU samples to test buildFgData");

    const data = buildFgData(cpuSamples, trace.callframeSymbols);
    if (!data) fail("buildFgData returned null for non-empty samples");
    if (data.totalSamples !== cpuSamples.length)
      fail(`totalSamples ${data.totalSamples} != ${cpuSamples.length}`);
    pass(
      `buildFgData: ${data.nodes.length} nodes, ${data.totalSamples} samples`
    );
  }

  function testBuildFgDataEmpty() {
    const data = buildFgData([], trace.callframeSymbols);
    if (data !== null) fail("buildFgData should return null for empty samples");
    pass("buildFgData returns null for empty samples");
  }

  // ── Run all tests ──

  console.log("\nbuildWorkerSpans:");
  testPollsHaveValidRange();
  testNoOverlappingPolls();
  testActiveRatiosInRange();
  testParksHaveValidRange();
  testQueueSamplesExist();

  console.log("\nattachCpuSamples:");
  testAttachedSamplesWithinPollBounds();
  testCpuResultCounts();

  console.log("\nextractLocalQueueSamples:");
  testLocalQueueNonNegative();
  testMaxLocalQueue();

  console.log("\nbuildActiveTaskTimeline:");
  testTimelineSorted();
  testCountNonNegative();

  console.log("\nindexWakeEvents:");
  testWakesByTaskSorted();
  testWakesByWorkerSorted();
  testWakeCountsConsistent();

  console.log("\ncomputeSchedulingDelays:");
  testDelaysPositive();
  testDelaysBounded();
  testWakeBeforePoll();
  testDelaysSorted();

  console.log("\nfilterPointsOfInterest:");
  testLongPollFilter();
  testCpuSampledFilter();
  testWakeDelayFilter();
  testSortByWorst();

  console.log("\nflamegraph:");
  testFlamegraphTree();
  testFlattenFlamegraph();
  testBuildFgData();
  testBuildFgDataEmpty();

  console.log("\n✓ All analysis checks passed!");
}

main().catch((e) => {
  console.error(e);
  process.exit(1);
});
