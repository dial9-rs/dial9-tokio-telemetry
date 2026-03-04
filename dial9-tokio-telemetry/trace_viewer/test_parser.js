#!/usr/bin/env node
// Simple test to verify JS parser matches Rust parser output

const fs = require("fs");
const { parseTrace, parseMetriqueData } = require("./trace_parser.js");

function main() {
    const args = process.argv.slice(2);

    // If no args, run unit tests
    if (args.length === 0) {
        runMetriqueTests();
        return;
    }

    if (args.length < 2) {
        console.error(
            "Usage: node test_parser.js [<trace.bin> <expected.jsonl>]",
        );
        console.error("");
        console.error(
            "With no args: runs built-in unit tests",
        );
        console.error(
            "With args: compares JS parser output against Rust trace_to_jsonl output",
        );
        process.exit(1);
    }

    const [tracePath, jsonlPath] = args;
    runComparisonTest(tracePath, jsonlPath);
}

function runMetriqueTests() {
    console.log("Running metrique event parsing tests...\n");
    let passed = 0, failed = 0;

    function assert(cond, msg) {
        if (!cond) { console.error(`  FAIL: ${msg}`); failed++; }
        else { passed++; }
    }

    // ── Test 1: parseMetriqueData with properties and metrics ──
    console.log("Test 1: parseMetriqueData basic");
    {
        const buf = buildMetriqueDataBlob({
            properties: [["status", "200"], ["path", "/api/test"]],
            metrics: [
                { key: "latency", isSpan: true, unit: "Millisecond", values: [42.5] },
                { key: "size", isSpan: false, unit: "Byte", values: [1024.0, 2048.0] },
            ],
        });
        const view = new DataView(buf.buffer, buf.byteOffset, buf.byteLength);
        const result = parseMetriqueData(view, buf);

        assert(result.properties.length === 2, "expected 2 properties");
        assert(result.properties[0].key === "status", "prop 0 key");
        assert(result.properties[0].value === "200", "prop 0 value");
        assert(result.properties[1].key === "path", "prop 1 key");
        assert(result.properties[1].value === "/api/test", "prop 1 value");

        assert(result.metrics.length === 2, "expected 2 metrics");
        assert(result.metrics[0].key === "latency", "metric 0 key");
        assert(result.metrics[0].isSpan === true, "metric 0 isSpan");
        assert(result.metrics[0].unit === "Millisecond", "metric 0 unit");
        assert(result.metrics[0].values.length === 1, "metric 0 values count");
        assert(Math.abs(result.metrics[0].values[0] - 42.5) < 0.001, "metric 0 value");

        assert(result.metrics[1].key === "size", "metric 1 key");
        assert(result.metrics[1].isSpan === false, "metric 1 isSpan");
        assert(result.metrics[1].unit === "Byte", "metric 1 unit");
        assert(result.metrics[1].values.length === 2, "metric 1 values count");
        assert(Math.abs(result.metrics[1].values[0] - 1024.0) < 0.001, "metric 1 value 0");
        assert(Math.abs(result.metrics[1].values[1] - 2048.0) < 0.001, "metric 1 value 1");
    }

    // ── Test 2: parseMetriqueData with empty properties ──
    console.log("Test 2: parseMetriqueData no properties");
    {
        const buf = buildMetriqueDataBlob({
            properties: [],
            metrics: [{ key: "count", isSpan: false, unit: "", values: [7.0] }],
        });
        const view = new DataView(buf.buffer, buf.byteOffset, buf.byteLength);
        const result = parseMetriqueData(view, buf);
        assert(result.properties.length === 0, "no properties");
        assert(result.metrics.length === 1, "one metric");
        assert(result.metrics[0].unit === null, "empty unit is null");
    }

    // ── Test 3: Full trace with metrique event ──
    console.log("Test 3: parseTrace with MetriqueEvent (wire code 11)");
    {
        const dataBlob = buildMetriqueDataBlob({
            properties: [["method", "GET"]],
            metrics: [{ key: "duration", isSpan: true, unit: "Millisecond", values: [15.3] }],
        });
        const traceBuf = buildMinimalTrace([
            buildMetriqueWireEvent(1000, 2, "RequestEntry", dataBlob),
        ]);
        const trace = parseTrace(traceBuf.buffer);
        assert(trace.metriqueEvents.length === 1, "one metrique event");
        const evt = trace.metriqueEvents[0];
        assert(evt.timestamp === 1000 * 1000, "timestamp in ns");
        assert(evt.workerId === 2, "worker id");
        assert(evt.entryName === "RequestEntry", "entry name");
        assert(evt.properties.length === 1, "one property");
        assert(evt.properties[0].key === "method", "property key");
        assert(evt.metrics.length === 1, "one metric");
        assert(evt.metrics[0].isSpan === true, "metric is span");
    }

    // ── Test 4: Multiple metrique events ──
    console.log("Test 4: parseTrace with multiple MetriqueEvents");
    {
        const d1 = buildMetriqueDataBlob({ properties: [], metrics: [{ key: "a", isSpan: false, unit: "", values: [1.0] }] });
        const d2 = buildMetriqueDataBlob({ properties: [], metrics: [{ key: "b", isSpan: true, unit: "Second", values: [2.0] }] });
        const traceBuf = buildMinimalTrace([
            buildMetriqueWireEvent(100, 0, "Entry1", d1),
            buildMetriqueWireEvent(200, 1, "Entry2", d2),
        ]);
        const trace = parseTrace(traceBuf.buffer);
        assert(trace.metriqueEvents.length === 2, "two metrique events");
        assert(trace.metriqueEvents[0].entryName === "Entry1", "first entry name");
        assert(trace.metriqueEvents[1].entryName === "Entry2", "second entry name");
    }

    console.log(`\n${passed} passed, ${failed} failed`);
    if (failed > 0) process.exit(1);
    console.log("✓ All metrique tests passed!");
}

// ── Helpers to build binary test data ──

function buildMetriqueDataBlob({ properties, metrics }) {
    const enc = new TextEncoder();
    const parts = [];

    // num_properties: u16
    parts.push(u16le(properties.length));
    for (const [key, value] of properties) {
        const kb = enc.encode(key), vb = enc.encode(value);
        parts.push(u16le(kb.length), kb, u16le(vb.length), vb);
    }

    // num_metrics: u16
    parts.push(u16le(metrics.length));
    for (const m of metrics) {
        const kb = enc.encode(m.key);
        parts.push(u16le(kb.length), kb);
        parts.push(new Uint8Array([m.isSpan ? 1 : 0])); // flags
        const ub = enc.encode(m.unit);
        parts.push(new Uint8Array([ub.length]), ub); // unit
        parts.push(u16le(m.values.length));
        for (const v of m.values) parts.push(f64le(v));
    }

    return concat(parts);
}

function buildMetriqueWireEvent(timestampUs, workerId, entryName, dataBlob, taskId = 0) {
    const enc = new TextEncoder();
    const nameBytes = enc.encode(entryName);
    const parts = [
        new Uint8Array([11]),           // wire code
        u32le(timestampUs),             // timestamp_us
        new Uint8Array([workerId]),     // worker_id
        u32le(taskId),                  // task_id
        u16le(nameBytes.length),        // entry_name_len
        nameBytes,                      // entry_name
        u32le(dataBlob.length),         // data_len
        dataBlob,                       // data
    ];
    return concat(parts);
}

function buildMinimalTrace(eventBuffers) {
    const enc = new TextEncoder();
    const magic = enc.encode("TOKIOTRC");
    const version = u32le(13);
    return concat([magic, version, ...eventBuffers]);
}

function u16le(v) { const b = new Uint8Array(2); new DataView(b.buffer).setUint16(0, v, true); return b; }
function u32le(v) { const b = new Uint8Array(4); new DataView(b.buffer).setUint32(0, v, true); return b; }
function f64le(v) { const b = new Uint8Array(8); new DataView(b.buffer).setFloat64(0, v, true); return b; }
function concat(arrays) {
    const total = arrays.reduce((s, a) => s + a.length, 0);
    const out = new Uint8Array(total);
    let off = 0;
    for (const a of arrays) { out.set(a, off); off += a.length; }
    return out;
}

function runComparisonTest(tracePath, jsonlPath) {

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
    for (const [addr, jsEntry] of trace.callframeSymbols) {
        const rustSymbol = rustCallframes.get(addr);
        // jsEntry is {symbol, location} object
        const jsSymbol = jsEntry.location
            ? `${jsEntry.symbol} @ ${jsEntry.location}`
            : jsEntry.symbol;
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
