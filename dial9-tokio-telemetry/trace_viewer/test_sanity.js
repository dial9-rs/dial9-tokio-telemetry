#!/usr/bin/env node
// Sanity test: verify demo-trace.bin can be parsed and contains expected data.
// Usage: node test_sanity.js [trace.bin]   (defaults to demo-trace.bin)

const fs = require("fs");
const path = require("path");
const zlib = require("zlib");
const { parseTrace } = require("./trace_parser.js");

let failures = 0;
function assert(condition, msg) {
    if (!condition) {
        console.error(`  ✗ ${msg}`);
        failures++;
    } else {
        console.log(`  ✓ ${msg}`);
    }
}

function main() {
    const tracePath = process.argv[2] || path.join(__dirname, "demo-trace.bin");

    if (!fs.existsSync(tracePath)) {
        console.error(`File not found: ${tracePath}`);
        process.exit(1);
    }

    console.log(`Parsing ${tracePath}...`);
    let rawBuf = fs.readFileSync(tracePath);
    // Auto-detect gzip (magic bytes 0x1f 0x8b) and decompress
    if (rawBuf[0] === 0x1f && rawBuf[1] === 0x8b) {
        console.log("  (gzip detected, decompressing...)");
        rawBuf = zlib.gunzipSync(rawBuf);
    }
    const buffer = rawBuf.buffer.slice(rawBuf.byteOffset, rawBuf.byteOffset + rawBuf.byteLength);
    const trace = parseTrace(buffer);

    console.log(`Parsed: ${trace.events.length} events, version ${trace.version}\n`);

    // --- Basic structure ---
    console.log("Basic structure:");
    assert(trace.magic === "D9TF", `magic = D9TF (got ${trace.magic})`);
    assert(trace.version > 0, `version > 0 (got ${trace.version})`);
    assert(trace.events.length > 100, `has >100 events (got ${trace.events.length})`);
    assert(!trace.truncated, "trace was not truncated by parser cap");

    // --- Expected event types ---
    const eventTypeCounts = {};
    for (const e of trace.events) {
        eventTypeCounts[e.eventType] = (eventTypeCounts[e.eventType] || 0) + 1;
    }

    // eventType mapping: 0=PollStart 1=PollEnd 2=WorkerPark 3=WorkerUnpark 4=QueueSample 9=WakeEvent
    console.log("\nEvent types present:");
    assert(eventTypeCounts[0] > 0, `PollStart events: ${eventTypeCounts[0] || 0}`);
    assert(eventTypeCounts[1] > 0, `PollEnd events: ${eventTypeCounts[1] || 0}`);
    assert(eventTypeCounts[2] > 0, `WorkerPark events: ${eventTypeCounts[2] || 0}`);
    assert(eventTypeCounts[3] > 0, `WorkerUnpark events: ${eventTypeCounts[3] || 0}`);
    assert(eventTypeCounts[4] > 0, `QueueSample events: ${eventTypeCounts[4] || 0}`);

    // PollStart and PollEnd should be balanced (within tolerance for trace boundaries)
    const pollStartCount = eventTypeCounts[0] || 0;
    const pollEndCount = eventTypeCounts[1] || 0;
    assert(
        Math.abs(pollStartCount - pollEndCount) < Math.max(pollStartCount, pollEndCount) * 0.05,
        `PollStart/PollEnd roughly balanced: ${pollStartCount} vs ${pollEndCount}`
    );

    // --- Multiple workers ---
    const workerIds = new Set();
    for (const e of trace.events) {
        if (e.eventType === 0) workerIds.add(e.workerId);
    }
    console.log("\nWorkers:");
    assert(workerIds.size >= 2, `multiple workers found: ${[...workerIds].sort().join(", ")}`);

    // --- Timestamps are reasonable (multi-worker traces are not strictly ordered) ---
    console.log("\nTimestamps:");

    const duration = trace.events[trace.events.length - 1].timestamp - trace.events[0].timestamp;
    assert(duration > 0, `trace spans ${(duration / 1e9).toFixed(2)}s`);

    // --- Task tracking ---
    console.log("\nTask tracking:");
    assert(trace.taskSpawnLocs.size > 0, `task spawn locations: ${trace.taskSpawnLocs.size}`);
    assert(trace.spawnLocations.size > 0, `unique spawn locations: ${trace.spawnLocations.size}`);

    // --- Summary ---
    console.log(`\n${failures === 0 ? "✓ All checks passed!" : `✗ ${failures} check(s) failed`}`);
    process.exit(failures > 0 ? 1 : 0);
}

main();
