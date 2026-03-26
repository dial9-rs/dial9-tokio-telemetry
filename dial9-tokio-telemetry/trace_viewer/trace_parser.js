// trace_parser.js - Binary trace parser using dial9-trace-format decoder
// Can be used in browser or Node.js

(function(exports) {
    'use strict';

    const MAX_EVENTS = 2_000_000; // cap parsed events to keep UI responsive

    function getTraceDecoder() {
        if (typeof require !== 'undefined') {
            const path = require('path');
            return require(path.resolve(__dirname, 'decode.js')).TraceDecoder;
        }
        // Browser: decode.js must be loaded before this script
        if (typeof TraceDecoder !== 'undefined') return TraceDecoder;
        throw new Error('TraceDecoder not found. Load decode.js before trace_parser.js');
    }

    /** Parse a string/bigint/number to a JS number */
    function num(v) {
        if (typeof v === 'number') return v;
        if (typeof v === 'string') return Number(v);
        if (typeof v === 'bigint') return Number(v);
        return 0;
    }

    /**
     * Parse a dial9-trace-format binary trace buffer.
     * @param {ArrayBuffer} buffer - The binary trace data
     * @returns {Object} Parsed trace with events, metadata, and CPU samples
     */
    function parseTrace(buffer, options) {
        const maxEvents = (options && options.maxEvents) || MAX_EVENTS;
        const TD = getTraceDecoder();
        const dec = new TD(buffer instanceof ArrayBuffer ? new Uint8Array(buffer) : buffer);
        if (!dec.decodeHeader()) throw new Error('Invalid trace header');
        const frames = dec.decodeAll();

        const events = [];
        const spawnLocations = new Map(); // string → string (identity, for compat)
        const taskSpawnLocs = new Map();  // taskId → spawn location string
        const taskSpawnTimes = new Map();
        const taskTerminateTimes = new Map();
        const callframeSymbols = new Map();
        const cpuSamples = [];
        const threadNames = new Map();
        const runtimeWorkers = new Map(); // runtime name → [workerId, ...]

        const capped = () => events.length >= maxEvents;
        // Frames processed regardless of event cap:
        // - SymbolTableEntry: symbol tables are needed to resolve addresses in all other frames
        // - TaskSpawnEvent/TaskTerminateEvent: task lifecycle tracking for the task list view
        // - CpuSampleEvent: CPU samples feed the flame graph, which should reflect the full trace
        const UNCAPPED_FRAMES = new Set([
            'TaskSpawnEvent', 'TaskTerminateEvent',
            'CpuSampleEvent', 'SymbolTableEntry',
            'SegmentMetadataEvent',
        ]);

        for (const frame of frames) {
            if (frame.type !== 'event') continue;
            const v = frame.values;
            const ts = num(frame.timestamp_ns);

            if (capped() && !UNCAPPED_FRAMES.has(frame.name)) continue;

            switch (frame.name) {
                case 'PollStartEvent': {
                    const spawnLoc = v.spawn_loc || null;
                    if (spawnLoc) spawnLocations.set(spawnLoc, spawnLoc);
                    const taskId = num(v.task_id);
                    if (taskId && spawnLoc && !taskSpawnLocs.has(taskId)) {
                        taskSpawnLocs.set(taskId, spawnLoc);
                    }
                    events.push({
                        eventType: 0, timestamp: ts,
                        workerId: num(v.worker_id), localQueue: num(v.local_queue),
                        globalQueue: 0, cpuTime: 0, schedWait: 0,
                        taskId, spawnLocId: spawnLoc, spawnLoc,
                    });
                    break;
                }
                case 'PollEndEvent':
                    events.push({
                        eventType: 1, timestamp: ts, workerId: num(v.worker_id),
                        globalQueue: 0, localQueue: 0, cpuTime: 0, schedWait: 0,
                        taskId: 0, spawnLocId: null, spawnLoc: null,
                    });
                    break;
                case 'WorkerParkEvent':
                    events.push({
                        eventType: 2, timestamp: ts,
                        workerId: num(v.worker_id), localQueue: num(v.local_queue),
                        cpuTime: num(v.cpu_time_ns),
                        globalQueue: 0, schedWait: 0,
                        taskId: 0, spawnLocId: null, spawnLoc: null,
                    });
                    break;
                case 'WorkerUnparkEvent':
                    events.push({
                        eventType: 3, timestamp: ts,
                        workerId: num(v.worker_id), localQueue: num(v.local_queue),
                        cpuTime: num(v.cpu_time_ns), schedWait: num(v.sched_wait_ns),
                        globalQueue: 0,
                        taskId: 0, spawnLocId: null, spawnLoc: null,
                    });
                    break;
                case 'QueueSampleEvent':
                    events.push({
                        eventType: 4, timestamp: ts,
                        globalQueue: num(v.global_queue),
                        workerId: 0, localQueue: 0, cpuTime: 0, schedWait: 0,
                        taskId: 0, spawnLocId: null, spawnLoc: null,
                    });
                    break;
                case 'TaskSpawnEvent': {
                    const taskId = num(v.task_id);
                    const spawnLoc = v.spawn_loc || null;
                    if (spawnLoc) spawnLocations.set(spawnLoc, spawnLoc);
                    taskSpawnLocs.set(taskId, spawnLoc);
                    taskSpawnTimes.set(taskId, ts);
                    break;
                }
                case 'TaskTerminateEvent':
                    taskTerminateTimes.set(num(v.task_id), ts);
                    break;
                case 'WakeEventEvent':
                    events.push({
                        eventType: 9, timestamp: ts,
                        workerId: num(v.target_worker),
                        wakerTaskId: num(v.waker_task_id),
                        wokenTaskId: num(v.woken_task_id),
                        targetWorker: num(v.target_worker),
                        globalQueue: 0, localQueue: 0, cpuTime: 0, schedWait: 0,
                        taskId: 0, spawnLocId: null, spawnLoc: null,
                    });
                    break;
                case 'CpuSampleEvent': {
                    const chain = (v.callchain || []).map(addr =>
                        "0x" + BigInt(addr).toString(16));
                    cpuSamples.push({
                        timestamp: ts, workerId: num(v.worker_id),
                        tid: num(v.tid), source: num(v.source), callchain: chain,
                    });
                    const tn = v.thread_name;
                    if (tn && tn !== '<no thread name>') {
                        threadNames.set(num(v.tid), tn);
                    }
                    break;
                }
                case 'SegmentMetadataEvent': {
                    const entries = v.entries || {};
                    for (const [key, val] of Object.entries(entries)) {
                        if (key.startsWith('runtime.')) {
                            const name = key.slice('runtime.'.length);
                            const ids = val.split(',').map(Number).filter(n => !isNaN(n));
                            if (ids.length > 0) runtimeWorkers.set(name, ids);
                        }
                    }
                    break;
                }
                case 'SymbolTableEntry': {
                    const addrKey = "0x" + BigInt(v.addr).toString(16);
                    const depth = Number(v.inline_depth || 0);
                    const sf = v.source_file || '';
                    const sl = Number(v.source_line || 0);
                    const location = sf ? (sl ? `${sf}:${sl}` : sf) : null;
                    const entry = { symbol: v.symbol_name, location };
                    if (depth === 0) {
                        // Outermost frame: store directly (or as first element of array)
                        const existing = callframeSymbols.get(addrKey);
                        if (Array.isArray(existing)) {
                            existing[0] = entry;
                        } else {
                            callframeSymbols.set(addrKey, entry);
                        }
                    } else {
                        // Inlined frame: promote to array
                        let arr = callframeSymbols.get(addrKey);
                        if (!Array.isArray(arr)) {
                            arr = [arr || { symbol: addrKey, location: null }];
                            callframeSymbols.set(addrKey, arr);
                        }
                        arr[depth] = entry;
                    }
                    break;
                }
            }
        }

        return {
            magic: 'D9TF', version: dec.version, events,
            truncated: events.length >= maxEvents,
            hasCpuTime: true, hasSchedWait: true, hasTaskTracking: true,
            spawnLocations, taskSpawnLocs, taskSpawnTimes,
            cpuSamples, callframeSymbols, threadNames, taskTerminateTimes,
            runtimeWorkers,
        };
    }

    // ── Symbol formatting utilities ──

    function _stripBoringGenerics(s) {
        const boring = /^[A-Z]$|^(Fut|Req|Res|Bs|InnerFuture)$/;
        return s.replace(/<([^<>]*)>/g, (match, inner) => {
            const params = inner.split(',').map(p => p.trim());
            if (params.every(p => boring.test(p))) return '';
            const kept = params.filter(p => !boring.test(p));
            return kept.length ? `<${kept.join(',')}>` : '';
        });
    }

    function _lastSeg(s) { return s.split('::').pop(); }

    function _shortenPath(s) {
        const parts = s.split('::');
        let closures = 0;
        for (let i = parts.length - 1; i >= 0; i--) {
            if (parts[i] === '{{closure}}') closures++; else break;
        }
        const meaningful = parts.length - closures;
        if (meaningful <= 3) return s;
        return parts.slice(meaningful - 3).join('::');
    }

    /**
     * Try to build a docs.rs source link from a location path containing a crate-version segment.
     * Matches any path like: .../hyper-0.14.28/src/client/connect/http.rs:474
     * Returns URL string or null.
     */
    function _docsRsUrl(location) {
        if (!location) return null;
        const m = location.match(/\/([a-z][a-z0-9_-]*)-(\d+\.\d+[^/]*)\/(.+?)(?::(\d+))?$/);
        if (!m) return null;
        const [, crate_, version, rawPath, line] = m;
        const crateSrc = crate_.replace(/-/g, '_');
        const path = rawPath.replace(/^src\//, '');
        let url = `https://docs.rs/${crate_}/${version}/src/${crateSrc}/${path}.html`;
        if (line) url += `#${line}`;
        return url;
    }

    /**
     * Extract just the filename from a location string.
     * e.g. "/home/user/.cargo/registry/src/.../hyper-0.14.28/src/client/connect/http.rs:474" → "http.rs"
     */
    function _fileName(location) {
        if (!location) return null;
        const m = location.match(/([^/]+\.rs)(?::\d+)?$/);
        return m ? m[1] : null;
    }

    /**
     * Format a stack frame for human-readable display.
     * Accepts either a resolved frame object or a raw address + callframeSymbols map.
     * @param {{symbol: string, location: string|null}|string} frame - Resolved frame or address string
     * @param {Map<string, {symbol: string, location: string|null}>} [callframeSymbols] - Required when frame is an address string
     * @returns {{text: string, docsUrl: string|null}}
     */
    function formatFrame(frame, callframeSymbols) {
        if (typeof frame === 'string') {
            if (!callframeSymbols) throw new Error('formatFrame requires callframeSymbols when given an address string');
            const entry = callframeSymbols.get(frame);
            if (!entry) return { text: frame || '(unknown)', docsUrl: null };
            frame = Array.isArray(entry) ? entry[0] : entry;
        }
        const { symbol: sym, location } = frame;
        if (!sym || sym.startsWith('0x')) return { text: sym || '(unknown)', docsUrl: null };

        let result = sym;
        const traitImplMatch = result.match(/^<(.+?) as (.+?)>::(.+)$/);
        if (traitImplMatch) {
            let [, implType, trait_, method] = traitImplMatch;
            const shortType = _lastSeg(_stripBoringGenerics(implType));
            result = shortType.length <= 2
                ? `${_lastSeg(_stripBoringGenerics(trait_))}::${method}`
                : `${shortType}::${method}`;
        } else if (result.includes('::')) {
            result = _shortenPath(_stripBoringGenerics(result));
        }

        const fileName = _fileName(location);
        if (location) {
            const m = location.match(/:(\d+)$/);
            if (m) result += ` ${fileName || ''}:${m[1]}`;
        }
        return { text: result, docsUrl: _docsRsUrl(location) };
    }

    /**
     * Resolve a callchain (array of address strings) to frame objects.
     * When an address has inlined frames (stored as an array in callframeSymbols),
     * they are expanded in place (outermost first, then inlined callees).
     * @param {string[]} callchain - Address strings like "0x55cc6d053893"
     * @param {Map<string, {symbol: string, location: string|null}|Array>} callframeSymbols
     * @returns {{symbol: string, location: string|null}[]}
     */
    function symbolizeChain(callchain, callframeSymbols) {
        const result = [];
        for (const addr of callchain) {
            const entry = callframeSymbols.get(addr);
            if (!entry) { result.push({ symbol: addr, location: null }); continue; }
            if (Array.isArray(entry)) { for (const e of entry) result.push(e); continue; }
            if (typeof entry === 'string') { result.push({ symbol: entry, location: null }); continue; }
            result.push(entry);
        }
        return result;
    }

    /**
     * Deduplicate CPU/sched samples by symbolized stack trace.
     * @param {Object[]} samples - Array of {callchain, ...} sample objects
     * @param {Map} callframeSymbols
     * @returns {{count: number, frames: Object[], leaf: string, leafRaw: string}[]}
     */
    function deduplicateSamples(samples, callframeSymbols) {
        const groups = new Map();
        for (const sample of samples) {
            const frames = symbolizeChain(sample.callchain, callframeSymbols);
            const key = frames.map(f => f.symbol).join('\0');
            if (!groups.has(key)) {
                groups.set(key, {
                    count: 0, frames,
                    leaf: frames[0] ? formatFrame(frames[0]).text : '(unknown)',
                    leafRaw: frames[0] ? frames[0].symbol : '',
                });
            }
            groups.get(key).count++;
        }
        return [...groups.values()].sort((a, b) => b.count - a.count);
    }

    // Export for both browser and Node.js
    if (typeof module !== 'undefined' && module.exports) {
        module.exports = { parseTrace, formatFrame, symbolizeChain, deduplicateSamples };
    } else {
        exports.TraceParser = { parseTrace, formatFrame, symbolizeChain, deduplicateSamples };
    }

})(typeof exports === 'undefined' ? this : exports);
