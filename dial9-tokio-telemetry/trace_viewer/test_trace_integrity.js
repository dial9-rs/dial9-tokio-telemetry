#!/usr/bin/env node
"use strict";

const fs = require("fs");
const path = require("path");
const zlib = require("zlib");
const { parseTrace } = require("./trace_parser.js");

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

// --- Parse ---

let rawBuf = fs.readFileSync(tracePath);
// Decompress gzip if needed (magic bytes 1f 8b)
if (rawBuf[0] === 0x1f && rawBuf[1] === 0x8b) {
  rawBuf = zlib.gunzipSync(rawBuf);
}
const buffer = rawBuf.buffer.slice(
  rawBuf.byteOffset,
  rawBuf.byteOffset + rawBuf.byteLength
);

function fail(msg) {
  console.log(`✗ ${msg}`);
  process.exit(1);
}

function pass(msg) {
  console.log(`✓ ${msg}`);
}

const trace = parseTrace(buffer);
console.log(`Parsed ${trace.events.length} events (version ${trace.version})`);

// --- Basic ---

console.log("\nBasic:");

if (trace.events.length === 0) fail("No events found");
pass(`Has ${trace.events.length} events`);

const EVENT_TYPES = {
  PollStart: 0,
  PollEnd: 1,
  WorkerPark: 2,
  WorkerUnpark: 3,
  QueueSample: 4,
  WakeEvent: 9,
};
const typeCounts = {};
trace.events.forEach((e) => {
  typeCounts[e.eventType] = (typeCounts[e.eventType] || 0) + 1;
});

for (const [name, type] of Object.entries(EVENT_TYPES)) {
  const count = typeCounts[type] || 0;

  if (!count) fail(`No ${name} events found`);
  else pass(`${name}: ${count} events`);
}

const eventsWithWorkerId = trace.events.filter(
  (e) =>
    e.eventType !== EVENT_TYPES.QueueSample &&
    e.eventType !== EVENT_TYPES.WakeEvent
);
const workerIds = [
  ...new Set(eventsWithWorkerId.map((e) => e.workerId)),
].sort();
// Demo trace uses a multi_thread runtime, so we expect at least 2 workers.
if (workerIds.length < 2) fail(`Only ${workerIds.length} worker(s)`);
else pass(`Multiple workers: ${JSON.stringify(workerIds)}`);

if (trace.truncated) fail("Trace was truncated at event cap");
else pass("Not truncated");

// --- Task tracking ---

console.log("\nTask tracking:");

if (!trace.taskSpawnLocs.size) fail("No task spawned");
else pass(`${trace.taskSpawnLocs.size} tasks spawned`);

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

// --- State machine (per worker) ---

console.log("\nState machine:");

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


// --- Field sanity ---

console.log("\nField sanity:");

const tsErrors = [];
for (const wid of workerIds) {
  const wEvents = trace.events.filter(
    (e) =>
      e.workerId === wid &&
      ![EVENT_TYPES.QueueSample, EVENT_TYPES.WakeEvent].includes(e.eventType)
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

const negQueue = trace.events.find(
  (e) => e.localQueue < 0 || e.globalQueue < 0
);
if (negQueue)
  fail(
    `Negative queue depth: type=${negQueue.eventType} localQueue=${negQueue.localQueue} globalQueue=${negQueue.globalQueue}`
  );
pass("Queue depths non-negative");

const maxWorkerId = Math.max(...workerIds);
if (maxWorkerId > 63) fail(`Unexpectedly large worker ID: ${maxWorkerId}`);
pass(`Worker IDs bounded [0, ${maxWorkerId}]`);

// --- Summary ---

console.log("\n✓ All checks passed!");
