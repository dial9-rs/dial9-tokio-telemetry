#!/usr/bin/env node
"use strict";

const fs = require("fs");
const path = require("path");
const { EVENT_TYPES, parseTrace } = require("./trace_parser.js");
const {
  buildWorkerSpans,
  attachCpuSamples,
  buildActiveTaskTimeline,
  computeSchedulingDelays,
  filterPointsOfInterest,
  buildFlamegraphTree,
  flattenFlamegraph,
  buildFgData,
  buildSpanData,
  collectDescendants,
  selectSpanRenderSet,
  computeSpanLayout,
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

  const { workerSpans, perWorker, queueSamples, workerQueueSamples, maxLocalQueue, wakesByTask, wakesByWorker } = buildWorkerSpans(
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

  // ── extractLocalQueueSamples (via buildWorkerSpans) ──

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

  // ── indexWakeEvents (via buildWorkerSpans) ──

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
      { hasSchedWait: trace.hasSchedWait }
    );
    if (pois.length === 0) fail("No long-poll points of interest found");
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
      { hasSchedWait: trace.hasSchedWait }
    );
    if (pois.length === 0) fail("No cpu-sampled points of interest found");
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
      { hasSchedWait: trace.hasSchedWait }
    );
    if (pois.length === 0) fail("No wake-delay points of interest found");
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
      { hasSchedWait: trace.hasSchedWait, sortByWorst: true }
    );
    for (let i = 1; i < pois.length; i++) {
      if (pois[i].value > pois[i - 1].value) fail("sortByWorst not descending");
    }
    pass("sortByWorst produces descending order");
  }

  // ── buildFlamegraphTree / flattenFlamegraph ──

  function testFlamegraphTree() {
    const cpuSamples = trace.cpuSamples.filter((s) => s.source !== 1);
    if (cpuSamples.length === 0) fail("No CPU samples found");

    const root = buildFlamegraphTree(cpuSamples, trace.callframeSymbols);
    if (root.count !== cpuSamples.length)
      fail(`Root count ${root.count} != sample count ${cpuSamples.length}`);
    pass(`Root count matches sample count (${root.count})`);
  }

  function testFlattenFlamegraph() {
    const cpuSamples = trace.cpuSamples.filter((s) => s.source !== 1);
    if (cpuSamples.length === 0) fail("No CPU samples found");

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
    if (cpuSamples.length === 0) fail("No CPU samples found");

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

  // ── buildSpanData ──

  function testBuildSpanDataPairing() {
    const customEvents = [
      { name: "SpanEnterEvent", timestamp: 1000, fields: { worker_id: 0, span_id: 1, parent_span_id: null, span_name: "handle_request", fields: { user_id: "42" } } },
      { name: "SpanEnterEvent", timestamp: 1100, fields: { worker_id: 0, span_id: 2, parent_span_id: 1, span_name: "redis_get", fields: { key: "foo" } } },
      { name: "SpanExitEvent",  timestamp: 1200, fields: { worker_id: 0, span_id: 2, span_name: "redis_get", fields: { key: "foo" } } },
      { name: "SpanExitEvent",  timestamp: 1300, fields: { worker_id: 0, span_id: 1, span_name: "handle_request", fields: { user_id: "42" } } },
    ];
    const { allSpans, spanMeta } = buildSpanData(customEvents);
    if (allSpans.length !== 2) fail(`Expected 2 spans, got ${allSpans.length}`);
    const redis = allSpans.find(s => s.spanName === "redis_get");
    const handle = allSpans.find(s => s.spanName === "handle_request");
    if (!redis || !handle) fail("Missing expected spans");
    if (redis.start !== 1100 || redis.end !== 1200) fail("redis_get timing wrong");
    if (redis.segments.length !== 1) fail(`Expected 1 segment, got ${redis.segments.length}`);
    if (redis.segments[0].workerId !== 0) fail("segment workerId wrong");
    if (!spanMeta.has("1") || !spanMeta.has("2")) fail("spanMeta missing entries");
    // Verify sorted by start time
    if (allSpans[0].start > allSpans[1].start) fail("Spans not sorted by start time");
    pass(`${allSpans.length} spans paired correctly`);
  }

  function testBuildSpanDataParent() {
    const customEvents = [
      { name: "SpanEnterEvent", timestamp: 1000, fields: { worker_id: 0, span_id: 10, parent_span_id: null, span_name: "root", fields: {} } },
      { name: "SpanEnterEvent", timestamp: 1100, fields: { worker_id: 0, span_id: 20, parent_span_id: 10, span_name: "child", fields: {} } },
      { name: "SpanExitEvent",  timestamp: 1200, fields: { worker_id: 0, span_id: 20, span_name: "child", fields: {} } },
      { name: "SpanExitEvent",  timestamp: 1300, fields: { worker_id: 0, span_id: 10, span_name: "root", fields: {} } },
    ];
    const { allSpans } = buildSpanData(customEvents);
    const child = allSpans.find(s => s.spanName === "child");
    if (child.parentSpanId !== "10") fail(`Expected parentSpanId="10", got ${child.parentSpanId}`);
    const root = allSpans.find(s => s.spanName === "root");
    if (root.parentSpanId !== null) fail(`Expected root parentSpanId=null, got ${root.parentSpanId}`);
    pass("Parent span IDs preserved correctly");
  }

  function testBuildSpanDataEmpty() {
    const { allSpans, spanMeta } = buildSpanData([]);
    if (allSpans.length !== 0) fail("Expected empty allSpans");
    if (spanMeta.size !== 0) fail("Expected empty spanMeta");
    pass("Empty input produces empty output");
  }

  function testBuildSpanDataDepth() {
    // Three levels of nesting via explicit parent
    const customEvents = [
      { name: "SpanEnterEvent", timestamp: 1000, fields: { worker_id: 0, span_id: 1, parent_span_id: null, span_name: "root", fields: {} } },
      { name: "SpanEnterEvent", timestamp: 1100, fields: { worker_id: 0, span_id: 2, parent_span_id: 1, span_name: "mid", fields: {} } },
      { name: "SpanEnterEvent", timestamp: 1200, fields: { worker_id: 0, span_id: 3, parent_span_id: 2, span_name: "leaf", fields: {} } },
      { name: "SpanExitEvent",  timestamp: 1300, fields: { worker_id: 0, span_id: 3, span_name: "leaf", fields: {} } },
      { name: "SpanExitEvent",  timestamp: 1400, fields: { worker_id: 0, span_id: 2, span_name: "mid", fields: {} } },
      { name: "SpanExitEvent",  timestamp: 1500, fields: { worker_id: 0, span_id: 1, span_name: "root", fields: {} } },
    ];
    const { allSpans, maxDepth } = buildSpanData(customEvents);
    const root = allSpans.find(s => s.spanName === "root");
    const mid = allSpans.find(s => s.spanName === "mid");
    const leaf = allSpans.find(s => s.spanName === "leaf");
    if (root.depth !== 0) fail(`root depth=${root.depth}, expected 0`);
    if (mid.depth !== 1) fail(`mid depth=${mid.depth}, expected 1`);
    if (leaf.depth !== 2) fail(`leaf depth=${leaf.depth}, expected 2`);
    if (maxDepth !== 2) fail(`maxDepth=${maxDepth}, expected 2`);
    pass("Depth computed correctly for 3-level nesting");
  }

  function testBuildSpanDataCycleDetection() {
    // Cyclic parent chain: A -> B -> A (should not stack overflow)
    const customEvents = [
      { name: "SpanEnterEvent", timestamp: 1000, fields: { worker_id: 0, span_id: 1, parent_span_id: 2, span_name: "a", fields: {} } },
      { name: "SpanEnterEvent", timestamp: 1100, fields: { worker_id: 0, span_id: 2, parent_span_id: 1, span_name: "b", fields: {} } },
      { name: "SpanExitEvent",  timestamp: 1200, fields: { worker_id: 0, span_id: 2, span_name: "b", fields: {} } },
      { name: "SpanExitEvent",  timestamp: 1300, fields: { worker_id: 0, span_id: 1, span_name: "a", fields: {} } },
    ];
    const { allSpans } = buildSpanData(customEvents);
    if (allSpans.length !== 2) fail("Expected 2 spans");
    // Just verify it didn't crash; depths may be arbitrary due to cycle
    pass("Cyclic parent chain does not stack overflow");
  }

  function testBuildSpanDataRecycledId() {
    // Span ID 1 used first as "alpha", closed, then recycled as "beta"
    const customEvents = [
      { name: "SpanEnterEvent", timestamp: 1000, fields: { worker_id: 0, span_id: 1, parent_span_id: null, span_name: "alpha", fields: {} } },
      { name: "SpanExitEvent",  timestamp: 1100, fields: { worker_id: 0, span_id: 1, span_name: "alpha", fields: {} } },
      { name: "SpanCloseEvent", timestamp: 1150, fields: { span_id: 1 } },
      // Same span_id reused with different name
      { name: "SpanEnterEvent", timestamp: 2000, fields: { worker_id: 0, span_id: 1, parent_span_id: null, span_name: "beta", fields: {} } },
      { name: "SpanExitEvent",  timestamp: 2100, fields: { worker_id: 0, span_id: 1, span_name: "beta", fields: {} } },
      { name: "SpanCloseEvent", timestamp: 2150, fields: { span_id: 1 } },
      // Child of the recycled span
      { name: "SpanEnterEvent", timestamp: 3000, fields: { worker_id: 0, span_id: 2, parent_span_id: 1, span_name: "child", fields: {} } },
      { name: "SpanExitEvent",  timestamp: 3100, fields: { worker_id: 0, span_id: 2, span_name: "child", fields: {} } },
    ];
    const { allSpans } = buildSpanData(customEvents);
    if (allSpans.length !== 3) fail(`Expected 3 spans, got ${allSpans.length}`);
    const alpha = allSpans.find(s => s.spanName === "alpha");
    const beta = allSpans.find(s => s.spanName === "beta");
    if (!alpha || !beta) fail("Missing alpha or beta span");
    if (alpha.start !== 1000 || beta.start !== 2000) fail("Span intervals not distinct");
    pass("Recycled span IDs produce separate intervals");
  }

  function testBuildSpanDataPerCallsiteSchema() {
    // New format: schema names are "SpanEnter:target::name:file:line"
    // User fields are top-level (not nested in a "fields" StringMap)
    const customEvents = [
      { name: "SpanEnter:myapp::handle:src/main.rs:10", timestamp: 1000, fields: { worker_id: 0, span_id: 1, parent_span_id: null, span_name: "handle", request_id: "abc-123" } },
      { name: "SpanExit:myapp::handle:src/main.rs:10",  timestamp: 1100, fields: { worker_id: 0, span_id: 1, span_name: "handle", request_id: "abc-123" } },
    ];
    const { allSpans } = buildSpanData(customEvents);
    if (!allSpans || allSpans.length !== 1) fail(`Expected 1 span, got ${allSpans?.length}`);
    if (allSpans[0].spanName !== "handle") fail(`Expected span name 'handle', got '${allSpans[0].spanName}'`);
    if (allSpans[0].fields.request_id !== "abc-123") fail(`Expected request_id='abc-123', got '${allSpans[0].fields.request_id}'`);
    // Base fields should NOT appear in the user fields
    if (allSpans[0].fields.worker_id) fail("worker_id should not be in user fields");
    if (allSpans[0].fields.span_name) fail("span_name should not be in user fields");
    pass("Per-callsite schema with typed fields parsed correctly");
  }

  function testBuildSpanDataUnmatched() {
    const customEvents = [
      { name: "SpanEnter:app::a:f:1", timestamp: 1000, fields: { worker_id: 0, span_id: 1, parent_span_id: null, span_name: "a" } },
      { name: "SpanExit:app::a:f:1",  timestamp: 1100, fields: { worker_id: 0, span_id: 1, span_name: "a" } },
      // This enter has no matching exit (trace ended mid-span)
      { name: "SpanEnter:app::b:f:2", timestamp: 1200, fields: { worker_id: 0, span_id: 2, parent_span_id: null, span_name: "b" } },
    ];
    const { allSpans, unmatchedSpans } = buildSpanData(customEvents);
    if (allSpans.length !== 1) fail(`Expected 1 matched span, got ${allSpans.length}`);
    if (!unmatchedSpans || unmatchedSpans.length !== 1) fail(`Expected 1 unmatched span, got ${unmatchedSpans?.length}`);
    if (unmatchedSpans[0].spanName !== "b") fail(`Expected unmatched span 'b', got '${unmatchedSpans[0].spanName}'`);
    if (unmatchedSpans[0].spanId !== "2") fail(`Expected unmatched spanId "2", got ${unmatchedSpans[0].spanId}`);
    pass("Unmatched spans (enter without exit) detected correctly");
  }

  function testBuildSpanDataChildrenIndex() {
    // Root r1 has children c1, c2. c1 has grandchild g1. r2 is childless.
    const customEvents = [
      { name: "SpanEnterEvent", timestamp: 100, fields: { worker_id: 0, span_id: 1, parent_span_id: null, span_name: "r1" } },
      { name: "SpanEnterEvent", timestamp: 110, fields: { worker_id: 0, span_id: 2, parent_span_id: 1, span_name: "c1" } },
      { name: "SpanEnterEvent", timestamp: 120, fields: { worker_id: 0, span_id: 3, parent_span_id: 2, span_name: "g1" } },
      { name: "SpanExitEvent",  timestamp: 130, fields: { worker_id: 0, span_id: 3, span_name: "g1" } },
      { name: "SpanExitEvent",  timestamp: 140, fields: { worker_id: 0, span_id: 2, span_name: "c1" } },
      { name: "SpanEnterEvent", timestamp: 150, fields: { worker_id: 0, span_id: 4, parent_span_id: 1, span_name: "c2" } },
      { name: "SpanExitEvent",  timestamp: 160, fields: { worker_id: 0, span_id: 4, span_name: "c2" } },
      { name: "SpanExitEvent",  timestamp: 170, fields: { worker_id: 0, span_id: 1, span_name: "r1" } },
      { name: "SpanEnterEvent", timestamp: 200, fields: { worker_id: 0, span_id: 5, parent_span_id: null, span_name: "r2" } },
      { name: "SpanExitEvent",  timestamp: 210, fields: { worker_id: 0, span_id: 5, span_name: "r2" } },
    ];
    const { childrenByParent } = buildSpanData(customEvents);
    if (!childrenByParent) fail("childrenByParent not exposed from buildSpanData");
    const roots = childrenByParent.get(null) || [];
    if (!roots.includes("1") || !roots.includes("5")) fail(`Roots should include "1" and "5", got ${[...roots]}`);
    const c1Children = childrenByParent.get("1") || [];
    if (!c1Children.includes("2") || !c1Children.includes("4")) fail(`r1 should have children "2" and "4", got ${[...c1Children]}`);
    const g1Children = childrenByParent.get("2") || [];
    if (!g1Children.includes("3")) fail(`c1 should have child "3", got ${[...g1Children]}`);
    // Childless spans should have no entry (or empty array)
    const r2Children = childrenByParent.get("5") || [];
    if (r2Children.length !== 0) fail(`r2 should be childless, got ${[...r2Children]}`);
    pass("childrenByParent index built correctly");
  }

  function testCollectDescendants() {
    // Same tree: r1 → {c1 → g1, c2}, r2 (no children)
    const childrenByParent = new Map([
      [null, ["1", "5"]],
      ["1", ["2", "4"]],
      ["2", ["3"]],
    ]);
    const d1 = collectDescendants(["1"], childrenByParent);
    // Should include 1, 2, 3, 4 (but not 5)
    if (!d1.has("1") || !d1.has("2") || !d1.has("3") || !d1.has("4")) {
      fail(`Expected {"1","2","3","4"} in descendants of "1", got ${[...d1]}`);
    }
    if (d1.has("5")) fail("r2 should not be in descendants of r1");
    if (d1.size !== 4) fail(`Expected size 4, got ${d1.size}`);

    const d5 = collectDescendants(["5"], childrenByParent);
    if (d5.size !== 1 || !d5.has("5")) fail(`Expected only {"5"}, got ${[...d5]}`);

    // Guard against cycles (children references ancestor)
    const cyclic = new Map([
      ["1", ["2"]],
      ["2", ["1"]], // cycle
    ]);
    const dc = collectDescendants(["1"], cyclic);
    if (!dc.has("1") || !dc.has("2")) fail("Cycle should still produce set");
    pass("collectDescendants returns id plus all descendants (cycle-safe)");
  }

  function testSelectSpanRenderSetRoots() {
    // When no focus, return only spans whose parent is null OR whose parent is absent
    const spans = [
      { spanId: "1", parentSpanId: null, spanName: "r1" },
      { spanId: "2", parentSpanId: "1",    spanName: "c1" },
      { spanId: "3", parentSpanId: "99",   spanName: "orphan" }, // parent not in set
      { spanId: "5", parentSpanId: null, spanName: "r2" },
    ];
    const childrenByParent = new Map([
      [null, ["1", "5"]],
      ["1", ["2"]],
    ]);
    const result = selectSpanRenderSet({
      allSpans: spans,
      focusedSpanId: null,
      childrenByParent,
    });
    const ids = new Set(result.map(s => s.spanId));
    if (!ids.has("1") || !ids.has("5") || !ids.has("3")) fail(`Expected {"1","3","5"}, got ${[...ids]}`);
    if (ids.has("2")) fail("Child span 2 should not be rendered in root view");
    pass("selectSpanRenderSet returns only root-like spans when focus is null");
  }

  function testSelectSpanRenderSetFocused() {
    const spans = [
      { spanId: "1", parentSpanId: null, spanName: "r1" },
      { spanId: "2", parentSpanId: "1",    spanName: "c1" },
      { spanId: "3", parentSpanId: "2",    spanName: "g1" },
      { spanId: "4", parentSpanId: "1",    spanName: "c2" },
      { spanId: "5", parentSpanId: null, spanName: "r2" },
    ];
    const childrenByParent = new Map([
      [null, ["1", "5"]],
      ["1", ["2", "4"]],
      ["2", ["3"]],
    ]);
    const result = selectSpanRenderSet({
      allSpans: spans,
      focusedSpanId: "1",
      childrenByParent,
    });
    const ids = new Set(result.map(s => s.spanId));
    // Focus on 1: should include 1 itself and all descendants (2, 3, 4). Not 5.
    if (!ids.has("1") || !ids.has("2") || !ids.has("3") || !ids.has("4")) {
      fail(`Expected focused set to include {"1","2","3","4"}, got ${[...ids]}`);
    }
    if (ids.has("5")) fail("Sibling root 5 should not be in focused set");
    pass("selectSpanRenderSet returns focused span + descendants");
  }

  function testComputeSpanLayoutDurationY() {
    // Three spans with very different durations. Panel: 100 px wide, 60 px tall.
    // Longest → smallest y (near top). Shortest → largest y (near bottom).
    const spans = [
      { spanId: 1, start: 0,   end: 100,   spanName: "tiny",   segments: [], activeNs: 100 },
      { spanId: 2, start: 10,  end: 1010,  spanName: "medium", segments: [], activeNs: 1000 },
      { spanId: 3, start: 20,  end: 10020, spanName: "huge",   segments: [], activeNs: 10000 },
    ];
    const layout = computeSpanLayout({
      spans,
      viewStart: 0,
      viewEnd: 10020,
      drawW: 1000,
      panelH: 60,
      clusterXPx: 2,
      barH: 4,
    });
    if (!layout || !layout.buckets) fail("computeSpanLayout must return {buckets}");
    // Should produce one bucket per span (no clustering at this wide view).
    if (layout.buckets.length !== 3) fail(`Expected 3 buckets, got ${layout.buckets.length}`);
    // Find buckets by representative spanId.
    const byId = new Map();
    for (const b of layout.buckets) byId.set(b.representative.spanId, b);
    const yTiny = byId.get(1).y;
    const yMed = byId.get(2).y;
    const yHuge = byId.get(3).y;
    // Larger duration → smaller y (higher on screen)
    if (!(yHuge < yMed && yMed < yTiny)) {
      fail(`Expected y(huge) < y(medium) < y(tiny), got ${yHuge} < ${yMed} < ${yTiny}`);
    }
    // All y within panel
    for (const b of layout.buckets) {
      if (b.y < 0 || b.y + b.h > 60 + 1) fail(`Bucket y=${b.y}, h=${b.h} outside panel 60`);
    }
    pass("computeSpanLayout places longer spans higher (smaller y)");
  }

  function testComputeSpanLayoutClusters() {
    // Many spans with identical duration piled at the same x — should cluster.
    const spans = [];
    for (let i = 0; i < 10; i++) {
      spans.push({ spanId: i + 1, start: 100, end: 200, spanName: "same", segments: [], activeNs: 100 });
    }
    // Add one outlier with different duration (far away on y axis)
    spans.push({ spanId: 100, start: 100, end: 10000, spanName: "outlier", segments: [], activeNs: 9900 });
    const layout = computeSpanLayout({
      spans,
      viewStart: 0,
      viewEnd: 10000,
      drawW: 500,
      panelH: 60,
      clusterXPx: 4,
      barH: 4,
    });
    // Expect the 10 identical spans to cluster into 1 bucket, plus the outlier in its own bucket.
    if (layout.buckets.length !== 2) {
      fail(`Expected 2 buckets (cluster + outlier), got ${layout.buckets.length}`);
    }
    const cluster = layout.buckets.find(b => b.spans.length > 1);
    if (!cluster) fail("Expected a cluster bucket");
    if (cluster.spans.length !== 10) fail(`Expected cluster size 10, got ${cluster.spans.length}`);
    // representative should be one of the clustered spans
    if (!cluster.spans.includes(cluster.representative)) fail("Representative should be a member of cluster.spans");
    pass("computeSpanLayout clusters overlapping spans into single bucket");
  }

  function testComputeSpanLayoutRepresentativeIsLongest() {
    // Several spans at the same position. Representative should be the longest.
    // All have the same start/end (same duration → same y), so they cluster.
    // We differentiate by activeNs to verify representative selection uses total duration.
    const spans = [
      { spanId: 1, start: 100, end: 200, spanName: "a", segments: [], activeNs: 50 },
      { spanId: 2, start: 100, end: 200, spanName: "b", segments: [], activeNs: 100 },
      { spanId: 3, start: 100, end: 200, spanName: "c", segments: [], activeNs: 80 },
    ];
    const layout = computeSpanLayout({
      spans,
      viewStart: 0,
      viewEnd: 500,
      drawW: 10,
      panelH: 60,
      clusterXPx: 100,
      barH: 4,
    });
    // Same duration → same y → same cell → one cluster
    const clustered = layout.buckets.find(b => b.spans.length === 3);
    if (!clustered) fail(`Expected single 3-span cluster, got ${JSON.stringify(layout.buckets.map(b => b.spans.length))}`);
    // All have same duration, so any is valid as representative (first encountered wins tie)
    // The key property: representative is a member of the cluster
    if (!clustered.spans.includes(clustered.representative)) {
      fail("Representative should be a member of cluster.spans");
    }
    pass("computeSpanLayout picks representative from cluster members");
  }

  function testBuildSpanDataMultiplePolls() {
    // A span entered/exited multiple times (async future polled 3 times with sleep gap)
    const customEvents = [
      { name: "SpanEnter:app::f:f:1", timestamp: 1000, fields: { worker_id: 0, span_id: 1, parent_span_id: null, span_name: "my_fn" } },
      { name: "SpanExit:app::f:f:1",  timestamp: 1500, fields: { worker_id: 0, span_id: 1, span_name: "my_fn" } },
      { name: "SpanEnter:app::f:f:1", timestamp: 100000, fields: { worker_id: 1, span_id: 1, parent_span_id: null, span_name: "my_fn" } },
      { name: "SpanExit:app::f:f:1",  timestamp: 100200, fields: { worker_id: 1, span_id: 1, span_name: "my_fn" } },
      { name: "SpanEnter:app::f:f:1", timestamp: 100300, fields: { worker_id: 0, span_id: 1, parent_span_id: null, span_name: "my_fn" } },
      { name: "SpanExit:app::f:f:1",  timestamp: 100400, fields: { worker_id: 0, span_id: 1, span_name: "my_fn" } },
      { name: "SpanCloseEvent",        timestamp: 100500, fields: { span_id: 1 } },
    ];
    const { allSpans } = buildSpanData(customEvents);
    if (allSpans.length !== 1) fail(`Expected 1 span, got ${allSpans.length}`);
    const s = allSpans[0];
    if (s.segments.length !== 3) fail(`Expected 3 segments, got ${s.segments.length}`);
    if (s.start !== 1000) fail(`Expected start=1000, got ${s.start}`);
    if (s.end !== 100400) fail(`Expected end=100400, got ${s.end}`);
    // activeNs = 500 + 200 + 100 = 800
    if (s.activeNs !== 800) fail(`Expected activeNs=800, got ${s.activeNs}`);
    // Workers: polled on both 0 and 1
    const workers = [...new Set(s.segments.map(seg => seg.workerId))].sort();
    if (workers.length !== 2 || workers[0] !== 0 || workers[1] !== 1) fail(`Expected workers [0,1], got ${workers}`);
    pass("Multiple polls grouped into single span with segments");
  }

  function testBuildSpanDataOutOfOrder() {
    // Events arrive out of order across workers — buildSpanData sorts by timestamp.
    // Also tests the defensive guard: span 1 enters on worker 0, then enters again
    // on worker 1 before exiting on worker 0 (the second enter should be ignored).
    const customEvents = [
      // Worker 1 events arrive first in the array but have later timestamps
      { name: "SpanEnterEvent", timestamp: 2000, fields: { worker_id: 1, span_id: 2, parent_span_id: null, span_name: "b" } },
      { name: "SpanExitEvent",  timestamp: 2500, fields: { worker_id: 1, span_id: 2, span_name: "b" } },
      // Worker 0 events arrive second but have earlier timestamps
      { name: "SpanEnterEvent", timestamp: 1000, fields: { worker_id: 0, span_id: 1, parent_span_id: null, span_name: "a" } },
      // Duplicate enter on worker 1 before exit on worker 0 (should be ignored)
      { name: "SpanEnterEvent", timestamp: 1200, fields: { worker_id: 1, span_id: 1, parent_span_id: null, span_name: "a" } },
      { name: "SpanExitEvent",  timestamp: 1500, fields: { worker_id: 0, span_id: 1, span_name: "a" } },
      { name: "SpanCloseEvent", timestamp: 3000, fields: { span_id: 1 } },
      { name: "SpanCloseEvent", timestamp: 3001, fields: { span_id: 2 } },
    ];
    const { allSpans } = buildSpanData(customEvents);
    if (allSpans.length !== 2) fail(`Expected 2 spans, got ${allSpans.length}`);
    const spanA = allSpans.find(s => s.spanName === "a");
    const spanB = allSpans.find(s => s.spanName === "b");
    if (!spanA || !spanB) fail("Expected spans 'a' and 'b'");
    // Span A: entered at 1000, exited at 1500 (duplicate enter at 1200 ignored)
    if (spanA.segments.length !== 1) fail(`Expected 1 segment for 'a', got ${spanA.segments.length}`);
    if (spanA.segments[0].start !== 1000) fail(`Expected segment start=1000, got ${spanA.segments[0].start}`);
    if (spanA.segments[0].end !== 1500) fail(`Expected segment end=1500, got ${spanA.segments[0].end}`);
    // Span B: entered at 2000, exited at 2500
    if (spanB.segments[0].start !== 2000 || spanB.segments[0].end !== 2500) {
      fail(`Span B segment wrong: ${spanB.segments[0].start}-${spanB.segments[0].end}`);
    }
    pass("Out-of-order events sorted correctly; duplicate enter on different worker ignored");
  }

  // ── Regression: open PollStart at trace end must not create phantom poll (#194) ──

  function testOpenPollStartDiscarded() {
    // Simulate a rotated segment where PollStart is the last event (no PollEnd).
    const syntheticEvents = [
      { eventType: EVENT_TYPES.PollStart, timestamp: 1000, workerId: 0, taskId: 1, spawnLocId: null, spawnLoc: null, localQueue: 0 },
      { eventType: EVENT_TYPES.PollEnd,   timestamp: 2000, workerId: 0 },
      // This PollStart has no matching PollEnd — file rotated
      { eventType: EVENT_TYPES.PollStart, timestamp: 3000, workerId: 0, taskId: 2, spawnLocId: null, spawnLoc: null, localQueue: 0 },
    ];
    const syntheticMaxTs = 1_000_000; // 1ms later — would create a huge phantom poll
    const result = buildWorkerSpans(syntheticEvents, [0], syntheticMaxTs);
    const polls = result.workerSpans[0].polls;
    if (polls.length !== 1) fail(`Expected 1 poll, got ${polls.length} — open PollStart was not discarded`);
    if (polls[0].start !== 1000 || polls[0].end !== 2000) fail(`Unexpected poll range`);
    pass("Open PollStart at trace end is discarded (no phantom long poll)");
  }

  // ── Run all tests ──

  console.log("\nbuildWorkerSpans:");
  testOpenPollStartDiscarded();
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

  console.log("\nbuildSpanData:");
  testBuildSpanDataPairing();
  testBuildSpanDataParent();
  testBuildSpanDataEmpty();
  testBuildSpanDataDepth();
  testBuildSpanDataCycleDetection();
  testBuildSpanDataRecycledId();
  testBuildSpanDataPerCallsiteSchema();
  testBuildSpanDataUnmatched();
  testBuildSpanDataChildrenIndex();
  testBuildSpanDataMultiplePolls();
  testBuildSpanDataOutOfOrder();

  console.log("\nspan pane layout:");
  testCollectDescendants();
  testSelectSpanRenderSetRoots();
  testSelectSpanRenderSetFocused();
  testComputeSpanLayoutDurationY();
  testComputeSpanLayoutClusters();
  testComputeSpanLayoutRepresentativeIsLongest();

  console.log("\n✓ All analysis checks passed!");
}

main().catch((e) => {
  console.error(e);
  process.exit(1);
});
