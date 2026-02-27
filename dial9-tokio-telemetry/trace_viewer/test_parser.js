#!/usr/bin/env node
// Simple test to verify JS parser matches Rust parser output

const fs = require("fs");
const { parseTrace } = require("./trace_parser.js");

function main() {
    const args = process.argv.slice(2);
    if (args.length < 2) {
        console.error(
            "Usage: node test_parser.js <trace.bin> <expected.jsonl>",
        );
        console.error("");
        console.error(
            "Compares JS parser output against Rust trace_to_jsonl output",
        );
        process.exit(1);
    }

    const [tracePath, jsonlPath] = args;

    // Parse binary trace with JS parser
    console.log(`Parsing ${tracePath}...`);
    const rawBuf = fs.readFileSync(tracePath);
    const buffer = rawBuf.buffer.slice(
        rawBuf.byteOffset,
        rawBuf.byteOffset + rawBuf.byteLength,
    );
    const trace = parseTrace(buffer);

    console.log(
        `Parsed ${trace.events.length} events (version ${trace.version})`,
    );
    console.log(`  - ${trace.spawnLocations.size} spawn locations`);
    console.log(`  - ${trace.taskSpawnLocs.size} task spawns`);
    console.log(`  - ${trace.cpuSamples.length} CPU samples`);
    console.log(`  - ${trace.callframeSymbols.size} callframe symbols`);

    // Read expected JSONL from Rust parser
    console.log(`\nReading expected output from ${jsonlPath}...`);
    const jsonl = fs.readFileSync(jsonlPath, "utf8");
    const expectedEvents = jsonl
        .trim()
        .split("\n")
        .filter((line) => line.trim())
        .map((line) => JSON.parse(line));

    console.log(`Expected ${expectedEvents.length} events`);

    // Count event types in both
    const jsEventCounts = {};
    const rustEventCounts = {};

    trace.events.forEach((e) => {
        const type = e.eventType;
        jsEventCounts[type] = (jsEventCounts[type] || 0) + 1;
    });

    expectedEvents.forEach((e) => {
        const type = e.event;
        rustEventCounts[type] = (rustEventCounts[type] || 0) + 1;
    });

    console.log("\nEvent counts (JS parser):");
    Object.entries(jsEventCounts)
        .sort()
        .forEach(([type, count]) => {
            console.log(`  ${type}: ${count}`);
        });

    console.log("\nEvent counts (Rust parser):");
    Object.entries(rustEventCounts)
        .sort()
        .forEach(([type, count]) => {
            console.log(`  ${type}: ${count}`);
        });

    // Check CallframeDef symbols match
    const rustCallframes = new Map();
    expectedEvents
        .filter((e) => e.event === "CallframeDef")
        .forEach((e) => {
            const addr = `0x${e.address.toString(16)}`;
            const combined = e.location
                ? `${e.symbol} @ ${e.location}`
                : e.symbol;
            rustCallframes.set(addr, combined);
        });

    console.log(`\nCallframe symbol comparison:`);
    console.log(`  JS: ${trace.callframeSymbols.size} symbols`);
    console.log(`  Rust: ${rustCallframes.size} symbols`);

    let mismatchCount = 0;
    for (const [addr, jsSymbol] of trace.callframeSymbols) {
        const rustSymbol = rustCallframes.get(addr);
        if (!rustSymbol) {
            console.log(`  MISSING in Rust: ${addr}`);
            mismatchCount++;
        } else if (jsSymbol !== rustSymbol) {
            console.log(`  MISMATCH ${addr}:`);
            console.log(`    JS:   ${jsSymbol}`);
            console.log(`    Rust: ${rustSymbol}`);
            mismatchCount++;
        }
    }

    if (mismatchCount === 0) {
        console.log("  ✓ All callframe symbols match!");
    } else {
        console.log(`  ✗ ${mismatchCount} mismatches found`);
        process.exit(1);
    }

    // Check CPU sample count matches
    const rustCpuSamples = expectedEvents.filter(
        (e) => e.event === "CpuSample",
    ).length;
    if (trace.cpuSamples.length === rustCpuSamples) {
        console.log(`\n✓ CPU sample count matches: ${rustCpuSamples}`);
    } else {
        console.log(`\n✗ CPU sample count mismatch:`);
        console.log(`  JS:   ${trace.cpuSamples.length}`);
        console.log(`  Rust: ${rustCpuSamples}`);
        process.exit(1);
    }

    console.log("\n✓ All checks passed!");
}

main();
