// Extract and test the JS binary parser against known Rust output
import { readFileSync } from 'fs';

function parseTrace(buffer) {
    const view = new DataView(buffer.buffer, buffer.byteOffset, buffer.byteLength);
    let off = 0;
    const magic = String.fromCharCode(...buffer.slice(0, 8));
    off += 8;
    const version = view.getUint32(off, true);
    off += 4;
    if (magic !== "TOKIOTRC") throw new Error("Not a TOKIOTRC file (got: " + magic + ")");
    const hasCpuTime = version >= 5;

    const events = [];
    while (off < buffer.byteLength) {
        if (off + 1 > buffer.byteLength) break;
        const wireCode = view.getUint8(off);
        off += 1;
        if (wireCode > 6) break;

        if (off + 4 > buffer.byteLength) break;
        const timestampUs = view.getUint32(off, true);
        off += 4;
        const timestamp = timestampUs * 1000;

        let eventType, workerId = 0, globalQueue = 0, localQueue = 0, cpuTime = 0;
        switch (wireCode) {
            case 0: case 1:
                if (off + 1 > buffer.byteLength) break;
                eventType = wireCode;
                workerId = view.getUint8(off); off += 1;
                break;
            case 2: case 3: {
                const need = hasCpuTime ? 6 : 2;
                if (off + need > buffer.byteLength) break;
                eventType = wireCode;
                workerId = view.getUint8(off); off += 1;
                localQueue = view.getUint8(off); off += 1;
                if (hasCpuTime) {
                    cpuTime = view.getUint32(off, true) * 1000;
                    off += 4;
                }
                break;
            }
            case 4:
                if (off + 1 > buffer.byteLength) break;
                eventType = 4;
                globalQueue = view.getUint8(off); off += 1;
                break;
            case 5:
                if (off + 2 > buffer.byteLength) break;
                eventType = 0;
                workerId = view.getUint8(off); off += 1;
                localQueue = view.getUint8(off); off += 1;
                break;
            case 6:
                if (off + 2 > buffer.byteLength) break;
                eventType = 1;
                workerId = view.getUint8(off); off += 1;
                localQueue = view.getUint8(off); off += 1;
                break;
        }
        events.push({ eventType, timestamp, workerId, globalQueue, localQueue, cpuTime });
    }
    return { magic, version, events };
}

const ET_NAMES = ["PollStart", "PollEnd", "WorkerPark", "WorkerUnpark", "QueueSample"];

// --- Test with synthetic v5 data ---
function testSyntheticV5() {
    // Build a minimal v5 trace in memory:
    // header(12) + PollStart lq=0 (6) + WorkerPark (11) + WorkerUnpark (11) + PollEnd lq=0 (6) + QueueSample (6)
    const buf = Buffer.alloc(12 + 6 + 11 + 11 + 6 + 6);
    let o = 0;
    // Header
    buf.write("TOKIOTRC", 0); o += 8;
    buf.writeUInt32LE(5, o); o += 4;
    // PollStart lq=0: code=0, ts=100, worker=2
    buf[o++] = 0; buf.writeUInt32LE(100, o); o += 4; buf[o++] = 2;
    // WorkerPark: code=2, ts=200, worker=1, lq=3, cpu_us=500
    buf[o++] = 2; buf.writeUInt32LE(200, o); o += 4; buf[o++] = 1; buf[o++] = 3; buf.writeUInt32LE(500, o); o += 4;
    // WorkerUnpark: code=3, ts=300, worker=1, lq=0, cpu_us=500
    buf[o++] = 3; buf.writeUInt32LE(300, o); o += 4; buf[o++] = 1; buf[o++] = 0; buf.writeUInt32LE(500, o); o += 4;
    // PollEnd lq=0: code=1, ts=400, worker=2
    buf[o++] = 1; buf.writeUInt32LE(400, o); o += 4; buf[o++] = 2;
    // QueueSample: code=4, ts=500, gq=7
    buf[o++] = 4; buf.writeUInt32LE(500, o); o += 4; buf[o++] = 7;

    const trace = parseTrace(buf);
    let ok = true;
    function check(cond, msg) { if (!cond) { console.log("  FAIL: " + msg); ok = false; } }

    check(trace.version === 5, `version=${trace.version}`);
    check(trace.events.length === 5, `count=${trace.events.length}`);

    const e = trace.events;
    check(e[0].eventType === 0, `e0 type=${e[0].eventType}`);
    check(e[0].workerId === 2, `e0 worker=${e[0].workerId}`);
    check(e[0].timestamp === 100000, `e0 ts=${e[0].timestamp}`);

    check(e[1].eventType === 2, `e1 type=${e[1].eventType}`);
    check(e[1].workerId === 1, `e1 worker=${e[1].workerId}`);
    check(e[1].localQueue === 3, `e1 lq=${e[1].localQueue}`);
    check(e[1].cpuTime === 500000, `e1 cpu=${e[1].cpuTime}`);

    check(e[2].eventType === 3, `e2 type=${e[2].eventType}`);
    check(e[2].workerId === 1, `e2 worker=${e[2].workerId}`);
    check(e[2].cpuTime === 500000, `e2 cpu=${e[2].cpuTime}`);

    check(e[3].eventType === 1, `e3 type=${e[3].eventType}`);
    check(e[3].workerId === 2, `e3 worker=${e[3].workerId}`);

    check(e[4].eventType === 4, `e4 type=${e[4].eventType}`);
    check(e[4].globalQueue === 7, `e4 gq=${e[4].globalQueue}`);

    console.log(ok ? "synthetic v5: PASS" : "synthetic v5: FAIL");
    return ok;
}

// --- Test with synthetic v4 data (no cpu_us on park/unpark) ---
function testSyntheticV4() {
    // header(12) + WorkerPark(7) + WorkerUnpark(7) + PollStart lq=0(6)
    const buf = Buffer.alloc(12 + 7 + 7 + 6);
    let o = 0;
    buf.write("TOKIOTRC", 0); o += 8;
    buf.writeUInt32LE(4, o); o += 4;
    // WorkerPark: code=2, ts=100, worker=0, lq=5
    buf[o++] = 2; buf.writeUInt32LE(100, o); o += 4; buf[o++] = 0; buf[o++] = 5;
    // WorkerUnpark: code=3, ts=200, worker=0, lq=0
    buf[o++] = 3; buf.writeUInt32LE(200, o); o += 4; buf[o++] = 0; buf[o++] = 0;
    // PollStart lq=0: code=0, ts=300, worker=0
    buf[o++] = 0; buf.writeUInt32LE(300, o); o += 4; buf[o++] = 0;

    const trace = parseTrace(buf);
    let ok = true;
    function check(cond, msg) { if (!cond) { console.log("  FAIL: " + msg); ok = false; } }

    check(trace.version === 4, `version=${trace.version}`);
    check(trace.events.length === 3, `count=${trace.events.length}`);

    const e = trace.events;
    check(e[0].eventType === 2, `e0 type=${e[0].eventType}`);
    check(e[0].workerId === 0, `e0 worker=${e[0].workerId}`);
    check(e[0].localQueue === 5, `e0 lq=${e[0].localQueue}`);
    check(e[0].cpuTime === 0, `e0 cpu=${e[0].cpuTime}`);

    check(e[1].eventType === 3, `e1 type=${e[1].eventType}`);
    check(e[1].workerId === 0, `e1 worker=${e[1].workerId}`);

    check(e[2].eventType === 0, `e2 type=${e[2].eventType}`);
    check(e[2].workerId === 0, `e2 worker=${e[2].workerId}`);
    check(e[2].timestamp === 300000, `e2 ts=${e[2].timestamp}`);

    console.log(ok ? "synthetic v4: PASS" : "synthetic v4: FAIL");
    return ok;
}

// --- Test with real file ---
function testFile(file) {
    const buf = readFileSync(file);
    const trace = parseTrace(buf);

    console.log(`\n${file}: v${trace.version}, ${trace.events.length} events`);

    const counts = {};
    for (const e of trace.events) {
        const name = ET_NAMES[e.eventType] || `Unknown(${e.eventType})`;
        counts[name] = (counts[name] || 0) + 1;
    }
    console.log("  counts:", counts);

    const workers = new Set(trace.events.filter(e => e.eventType !== 4).map(e => e.workerId));
    console.log("  workers:", [...workers].sort((a,b) => a-b));

    const undefs = trace.events.filter(e => e.eventType === undefined);
    if (undefs.length) console.log(`  ERROR: ${undefs.length} undefined eventTypes!`);
}

// Run tests
let allOk = testSyntheticV5() & testSyntheticV4() & testSpanBuilding();
process.exit(allOk ? 0 : 1);

// --- Test span building with out-of-order events ---
function testSpanBuilding() {
    // Simulate what the viewer does: events from two workers interleaved out of order
    const ET = { PollStart: 0, PollEnd: 1, WorkerPark: 2, WorkerUnpark: 3, QueueSample: 4 };
    const evts = [
        // Worker 0 buffer flush: park then later events
        { eventType: ET.WorkerPark, timestamp: 100, workerId: 0, cpuTime: 50, localQueue: 0, globalQueue: 0 },
        // Worker 1 buffer flush
        { eventType: ET.WorkerUnpark, timestamp: 80, workerId: 1, cpuTime: 20, localQueue: 0, globalQueue: 0 },
        { eventType: ET.WorkerPark, timestamp: 200, workerId: 1, cpuTime: 100, localQueue: 0, globalQueue: 0 },
        // Worker 0 next flush: unpark came after park in stream but before in time
        { eventType: ET.WorkerUnpark, timestamp: 150, workerId: 0, cpuTime: 50, localQueue: 0, globalQueue: 0 },
        { eventType: ET.WorkerPark, timestamp: 300, workerId: 0, cpuTime: 180, localQueue: 0, globalQueue: 0 },
    ];

    // Group and sort per worker
    const perWorker = {};
    for (const e of evts) {
        if (e.eventType === ET.QueueSample) continue;
        (perWorker[e.workerId] ??= []).push(e);
    }
    for (const wEvents of Object.values(perWorker)) {
        wEvents.sort((a, b) => a.timestamp - b.timestamp);
    }

    // Build active spans
    const actives = {};
    const openUnpark = {}, openPark = {};
    for (const [w, wEvents] of Object.entries(perWorker)) {
        actives[w] = [];
        for (const e of wEvents) {
            if (e.eventType === ET.WorkerPark) {
                openPark[w] = e.timestamp;
                if (openUnpark[w] != null) {
                    const wallDelta = e.timestamp - openUnpark[w].timestamp;
                    const cpuDelta = e.cpuTime - openUnpark[w].cpuTime;
                    const ratio = wallDelta > 0 ? Math.min(cpuDelta / wallDelta, 1.0) : 1.0;
                    actives[w].push({ start: openUnpark[w].timestamp, end: e.timestamp, ratio });
                    openUnpark[w] = null;
                }
            } else if (e.eventType === ET.WorkerUnpark) {
                openUnpark[w] = { timestamp: e.timestamp, cpuTime: e.cpuTime };
            }
        }
    }

    let ok = true;
    function check(cond, msg) { if (!cond) { console.log("  FAIL: " + msg); ok = false; } }

    // Worker 0: unpark@150(cpu=50) → park@300(cpu=180) → ratio = 130/150 = 0.867
    check(actives[0].length === 1, `w0 actives=${actives[0].length}`);
    check(actives[0][0].start === 150, `w0 start=${actives[0][0].start}`);
    check(actives[0][0].end === 300, `w0 end=${actives[0][0].end}`);
    check(Math.abs(actives[0][0].ratio - 130/150) < 0.01, `w0 ratio=${actives[0][0].ratio}`);

    // Worker 1: unpark@80(cpu=20) → park@200(cpu=100) → ratio = 80/120 = 0.667
    check(actives[1].length === 1, `w1 actives=${actives[1].length}`);
    check(Math.abs(actives[1][0].ratio - 80/120) < 0.01, `w1 ratio=${actives[1][0].ratio}`);

    console.log(ok ? "span building: PASS" : "span building: FAIL");
    return ok;
}
