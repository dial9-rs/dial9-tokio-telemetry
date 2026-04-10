#!/usr/bin/env node
// Simple test to verify JS parser matches Rust parser output

const fs = require("fs");
const { parseTrace } = require("./trace_parser.js");

async function main() {
    const args = process.argv.slice(2);
    if (args.length < 2) {
        console.error("Usage: node test_parser.js <trace.bin> <expected.jsonl>");
        process.exit(1);
    }

    const [tracePath, jsonlPath] = args;

    // Parse binary trace with JS parser
    console.log(`Parsing ${tracePath}...`);
    const trace = await parseTrace(fs.readFileSync(tracePath));

    console.log(`Parsed ${trace.events.length} events (version ${trace.version})`);
    console.log(`  - ${trace.spawnLocations.size} spawn locations`);
    console.log(`  - ${trace.taskSpawnLocs.size} task spawns`);
    console.log(`  - ${trace.cpuSamples.length} CPU samples`);
    console.log(`  - ${trace.callframeSymbols.size} callframe symbols`);

    // Read expected JSONL from Rust parser
    console.log(`\nReading expected output from ${jsonlPath}...`);
    const jsonl = fs.readFileSync(jsonlPath, "utf8");
    const expectedEvents = jsonl.trim().split("\n").filter(l => l.trim()).map(l => JSON.parse(l));
    console.log(`Expected ${expectedEvents.length} events`);

    // Map Rust event names to JS eventType numbers
    const rustNameToType = {
        PollStart: 0, PollEnd: 1, WorkerPark: 2, WorkerUnpark: 3,
        QueueSample: 4, WakeEvent: 9,
    };

    // Count runtime events (skip metadata-only events)
    const rustRuntimeEvents = expectedEvents.filter(e => e.event in rustNameToType);
    const jsEventCounts = {};
    const rustEventCounts = {};

    trace.events.forEach(e => { jsEventCounts[e.eventType] = (jsEventCounts[e.eventType] || 0) + 1; });
    rustRuntimeEvents.forEach(e => {
        const t = rustNameToType[e.event];
        rustEventCounts[t] = (rustEventCounts[t] || 0) + 1;
    });

    console.log("\nEvent counts (JS):", JSON.stringify(jsEventCounts));
    console.log("Event counts (Rust):", JSON.stringify(rustEventCounts));

    // Check event counts match
    let countMismatch = false;
    for (const t of new Set([...Object.keys(jsEventCounts), ...Object.keys(rustEventCounts)])) {
        if ((jsEventCounts[t] || 0) !== (rustEventCounts[t] || 0)) {
            console.log(`  ✗ Type ${t}: JS=${jsEventCounts[t] || 0} Rust=${rustEventCounts[t] || 0}`);
            countMismatch = true;
        }
    }
    if (countMismatch) { console.log("Event count mismatch"); process.exit(1); }
    console.log("✓ Event counts match");

    // Check CallframeDef symbols match
    const rustCallframes = new Map();
    expectedEvents.filter(e => e.event === "CallframeDef").forEach(e => {
        const addr = `0x${e.address.toString(16)}`;
        rustCallframes.set(addr, e.location ? `${e.symbol} @ ${e.location}` : e.symbol);
    });

    console.log(`\nCallframe symbols: JS=${trace.callframeSymbols.size} Rust=${rustCallframes.size}`);
    let mismatchCount = 0;
    for (const [addr, jsEntry] of trace.callframeSymbols) {
        const rustSymbol = rustCallframes.get(addr);
        const jsSymbol = jsEntry.location ? `${jsEntry.symbol} @ ${jsEntry.location}` : jsEntry.symbol;
        if (!rustSymbol) { console.log(`  MISSING in Rust: ${addr}`); mismatchCount++; }
        else if (jsSymbol !== rustSymbol) {
            console.log(`  MISMATCH ${addr}:\n    JS:   ${jsSymbol}\n    Rust: ${rustSymbol}`);
            mismatchCount++;
        }
    }
    if (mismatchCount > 0) { console.log(`✗ ${mismatchCount} callframe mismatches`); process.exit(1); }
    console.log("✓ All callframe symbols match");

    // Check CPU sample count
    const rustCpuSamples = expectedEvents.filter(e => e.event === "CpuSample").length;
    if (trace.cpuSamples.length !== rustCpuSamples) {
        console.log(`\n✗ CPU sample count: JS=${trace.cpuSamples.length} Rust=${rustCpuSamples}`);
        process.exit(1);
    }
    console.log(`✓ CPU sample count matches: ${rustCpuSamples}`);

    // Check spawn locations are resolved on PollStart events
    const pollStarts = trace.events.filter(e => e.eventType === 0);
    const withSpawnLoc = pollStarts.filter(e => e.spawnLoc !== null);
    console.log(`\nSpawn locations: ${withSpawnLoc.length}/${pollStarts.length} PollStart events have spawnLoc`);
    if (pollStarts.length > 0 && withSpawnLoc.length === 0) {
        console.log("✗ No PollStart events have spawn locations resolved — field name mismatch?");
        process.exit(1);
    }
    console.log("✓ Spawn locations resolved");

    // Spot-check: compare first few runtime events field-by-field
    let spotErrors = 0;
    const jsIdx = { i: 0 };
    for (const re of rustRuntimeEvents.slice(0, 50)) {
        const je = trace.events[jsIdx.i++];
        if (!je) { spotErrors++; continue; }
        const expectedType = rustNameToType[re.event];
        if (je.eventType !== expectedType) {
            console.log(`  Event ${jsIdx.i - 1}: type JS=${je.eventType} Rust=${expectedType}`);
            spotErrors++;
            continue;
        }
        if (je.timestamp !== re.timestamp_ns) {
            console.log(`  Event ${jsIdx.i - 1} (${re.event}): timestamp JS=${je.timestamp} Rust=${re.timestamp_ns}`);
            spotErrors++;
        }
    }
    if (spotErrors > 0) { console.log(`✗ ${spotErrors} spot-check errors`); process.exit(1); }
    console.log("✓ Spot-check passed");

    console.log("\n✓ All checks passed!");
}

main().catch((e) => { console.error(e); process.exit(1); });
