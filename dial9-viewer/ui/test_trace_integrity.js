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

  const stat = fs.statSync(tracePath);
  console.log(`Found trace: ${tracePath} (${stat.size} bytes)`);

  if (stat.size === 0) {
    console.error("Trace file is empty");
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
  console.log(
    `Parsed ${trace.events.length} events (version ${trace.version})`
  );

  function testHasEvents() {
    if (trace.events.length === 0) fail("No events found");
    pass(`Has ${trace.events.length} events`);
  }

  function testAllEventTypesPresent() {
    const typeCounts = {};
    trace.events.forEach((e) => {
      typeCounts[e.eventType] = (typeCounts[e.eventType] || 0) + 1;
    });

    for (const [name, type] of Object.entries(EVENT_TYPES)) {
      const count = typeCounts[type] || 0;
      if (!count) fail(`No ${name} events found`);
      else pass(`${name}: ${count} events`);
    }
  }

  function testMultipleWorkers() {
    const eventsWithWorkerId = trace.events.filter(
      (e) =>
        e.eventType !== EVENT_TYPES.QueueSample &&
        e.eventType !== EVENT_TYPES.WakeEvent
    );
    const workerIds = [
      ...new Set(eventsWithWorkerId.map((e) => e.workerId)),
    ].sort();
    if (workerIds.length < 2) fail(`Only ${workerIds.length} worker(s)`);
    else pass(`Multiple workers: ${JSON.stringify(workerIds)}`);
  }

  function testNotTruncated() {
    if (trace.truncated) fail("Trace was truncated at event cap");
    else pass("Not truncated");
  }

  function testTasksSpawned() {
    if (!trace.taskSpawnLocs.size) fail("No task spawned");
    else pass(`${trace.taskSpawnLocs.size} tasks spawned`);
  }

  function testSpawnLocationsResolved() {
    const pollStarts = trace.events.filter(
      (e) => e.eventType === EVENT_TYPES.PollStart
    );
    const withSpawnLoc = pollStarts.filter((e) => !!e.spawnLoc);

    if (pollStarts.length > 0 && withSpawnLoc.length === 0)
      fail("No PollStart has spawnLoc");
    else
      pass(
        `Spawn locations resolved: ${withSpawnLoc.length}/${pollStarts.length} PollStart events`
      );
  }

  function testAllPolledTasksWereSpawned() {
    const pollStarts = trace.events.filter(
      (e) => e.eventType === EVENT_TYPES.PollStart
    );
    const unspawnedTasks = [];
    for (const e of pollStarts) {
      if (e.taskId && !trace.taskSpawnLocs.has(e.taskId)) {
        unspawnedTasks.push(e.taskId);
      }
    }
    if (unspawnedTasks.length > 0)
      fail(
        `${
          unspawnedTasks.length
        } task(s) polled but never spawned: ${unspawnedTasks.join(", ")}`
      );
    pass("All polled tasks were spawned");
  }

  function testTaskLifecycleConsistency() {
    const lifecycleErrors = [];
    for (const [taskId, spawnTime] of trace.taskSpawnTimes) {
      const termTime = trace.taskTerminateTimes.get(taskId);
      if (termTime !== undefined && termTime < spawnTime) {
        lifecycleErrors.push(taskId);
      }
    }
    if (lifecycleErrors.length)
      fail(`${lifecycleErrors.length} task(s) terminated before spawn`);
    else pass("Task lifecycle consistent (spawn < terminate)");
  }

  function testPollStartEndPairing() {
    const workerIds = getWorkerIds();
    const pollErrors = [];
    for (const wid of workerIds) {
      const wEvents = trace.events.filter(
        (e) =>
          e.workerId === wid &&
          [EVENT_TYPES.PollStart, EVENT_TYPES.PollEnd].includes(e.eventType)
      );
      const bad = wEvents.find(
        (e, i) => i > 0 && e.eventType === wEvents[i - 1].eventType
      );
      if (bad) {
        pollErrors.push(
          `worker ${wid}: duplicate ${
            bad.eventType === EVENT_TYPES.PollStart ? "PollStart" : "PollEnd"
          } at ts=${bad.timestamp}`
        );
      }
    }
    if (pollErrors.length > 0) fail(pollErrors[0]);
    pass("PollStart/PollEnd pairing (no nested polls)");
  }

  function testWorkerParkUnparkPairing() {
    const workerIds = getWorkerIds();
    const parkErrors = [];
    for (const wid of workerIds) {
      const wEvents = trace.events.filter(
        (e) =>
          e.workerId === wid &&
          [EVENT_TYPES.WorkerPark, EVENT_TYPES.WorkerUnpark].includes(
            e.eventType
          )
      );
      const bad = wEvents.find(
        (e, i) => i > 0 && e.eventType === wEvents[i - 1].eventType
      );
      if (bad) {
        parkErrors.push(
          `worker ${wid}: duplicate ${
            bad.eventType === EVENT_TYPES.WorkerPark
              ? "WorkerPark"
              : "WorkerUnpark"
          } at ts=${bad.timestamp}`
        );
      }
    }
    if (parkErrors.length > 0) fail(parkErrors[0]);
    pass("WorkerPark/WorkerUnpark pairing (no double park)");
  }

  function testTimestampsIncreasing() {
    const workerIds = getWorkerIds();
    const tsErrors = [];
    for (const wid of workerIds) {
      const wEvents = trace.events.filter(
        (e) =>
          e.workerId === wid &&
          ![EVENT_TYPES.QueueSample, EVENT_TYPES.WakeEvent].includes(
            e.eventType
          )
      );
      for (let i = 1; i < wEvents.length; i++) {
        if (wEvents[i].timestamp < wEvents[i - 1].timestamp) {
          tsErrors.push(
            `worker ${wid}: ts ${wEvents[i].timestamp} < ${
              wEvents[i - 1].timestamp
            } at index ${i}`
          );
          break;
        }
      }
    }

    if (tsErrors.length > 0) fail(`Timestamps are decreasing: ${tsErrors[0]}`);
    pass("Timestamps are increasing per worker");
  }

  function testQueueDepthsNonNegative() {
    const negQueue = trace.events.find(
      (e) => e.localQueue < 0 || e.globalQueue < 0
    );
    if (negQueue)
      fail(
        `Negative queue depth: type=${negQueue.eventType} localQueue=${negQueue.localQueue} globalQueue=${negQueue.globalQueue}`
      );
    pass("Queue depths non-negative");
  }

  function testWorkerIdsBounded() {
    const workerIds = getWorkerIds();
    const maxWorkerId = Math.max(...workerIds);
    if (maxWorkerId > 63) fail(`Unexpectedly large worker ID: ${maxWorkerId}`);
    pass(`Worker IDs bounded [0, ${maxWorkerId}]`);
  }

  function testCpuTimeNonNegative() {
    const parks = trace.events.filter(
      (e) => e.eventType === EVENT_TYPES.WorkerPark
    );
    const unparks = trace.events.filter(
      (e) => e.eventType === EVENT_TYPES.WorkerUnpark
    );
    const negCpuTime = parks.concat(unparks).find((e) => e.cpuTime < 0);
    if (negCpuTime)
      fail(
        `Negative cpuTime: ${negCpuTime.cpuTime} at ts=${negCpuTime.timestamp}`
      );
    pass("cpuTime non-negative on Park/Unpark");
  }

  function testSchedWaitNonNegative() {
    const unparks = trace.events.filter(
      (e) => e.eventType === EVENT_TYPES.WorkerUnpark
    );
    const negSchedWait = unparks.find((e) => e.schedWait < 0);
    if (negSchedWait)
      fail(
        `Negative schedWait: ${negSchedWait.schedWait} at ts=${negSchedWait.timestamp}`
      );
    pass("schedWait non-negative on Unpark");
  }

  function testCpuTimePopulated() {
    const parks = trace.events.filter(
      (e) => e.eventType === EVENT_TYPES.WorkerPark
    );
    const unparks = trace.events.filter(
      (e) => e.eventType === EVENT_TYPES.WorkerUnpark
    );
    const nonZeroCpuTime = parks.concat(unparks).filter((e) => e.cpuTime > 0);
    if (nonZeroCpuTime.length === 0)
      fail("All cpuTime values are zero — instrumentation may be broken");
    pass(
      `cpuTime populated: ${nonZeroCpuTime.length}/${
        parks.length + unparks.length
      } events`
    );
  }

  function testWakeEventReferencesKnownTasks() {
    const wakeEvents = trace.events.filter(
      (e) => e.eventType === EVENT_TYPES.WakeEvent
    );
    const unknownWoken = wakeEvents.filter(
      (e) => e.wokenTaskId && !trace.taskSpawnLocs.has(e.wokenTaskId)
    );
    if (unknownWoken.length > 0)
      fail(
        `${unknownWoken.length} WakeEvent(s) reference unknown wokenTaskId (first: ${unknownWoken[0].wokenTaskId})`
      );
    pass("WakeEvent wokenTaskId references known tasks");
  }

  function testWakeEventTargetWorkerInRange() {
    const workerIds = getWorkerIds();
    const maxWorkerId = Math.max(...workerIds);
    const wakeEvents = trace.events.filter(
      (e) => e.eventType === EVENT_TYPES.WakeEvent
    );
    const outOfRangeWorker = wakeEvents.find(
      (e) =>
        e.targetWorker !== 255 && // UNKNOWN
        e.targetWorker !== 254 && // BLOCKING
        e.targetWorker > maxWorkerId
    );
    if (outOfRangeWorker)
      fail(
        `WakeEvent targetWorker ${outOfRangeWorker.targetWorker} exceeds max worker ID ${maxWorkerId}`
      );
    pass("WakeEvent targetWorker within valid range");
  }

  function testAllPollStartsHaveTaskId() {
    const pollStarts = trace.events.filter(
      (e) => e.eventType === EVENT_TYPES.PollStart
    );
    const zeroTaskPoll = pollStarts.find((e) => !e.taskId);
    if (zeroTaskPoll)
      fail(`PollStart with zero taskId at ts=${zeroTaskPoll.timestamp}`);
    pass("All PollStart events have a taskId");
  }

  function getWorkerIds() {
    const eventsWithWorkerId = trace.events.filter(
      (e) =>
        e.eventType !== EVENT_TYPES.QueueSample &&
        e.eventType !== EVENT_TYPES.WakeEvent
    );
    return [...new Set(eventsWithWorkerId.map((e) => e.workerId))].sort();
  }

  console.log("\nBasic:");
  testHasEvents();
  testAllEventTypesPresent();
  testMultipleWorkers();
  testNotTruncated();

  console.log("\nTask tracking:");
  testTasksSpawned();
  testSpawnLocationsResolved();
  testAllPolledTasksWereSpawned();
  testTaskLifecycleConsistency();

  console.log("\nState machine:");
  testPollStartEndPairing();
  testWorkerParkUnparkPairing();

  console.log("\nField sanity:");
  testTimestampsIncreasing();
  testQueueDepthsNonNegative();
  testWorkerIdsBounded();
  testCpuTimeNonNegative();
  testSchedWaitNonNegative();
  testCpuTimePopulated();
  testWakeEventReferencesKnownTasks();
  testWakeEventTargetWorkerInRange();
  testAllPollStartsHaveTaskId();

  console.log("\n✓ All checks passed!");
}

main().catch((e) => {
  console.error(e);
  process.exit(1);
});
