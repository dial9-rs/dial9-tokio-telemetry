// trace_parser.js - Binary trace parser for TOKIOTRC format
// Can be used in browser or Node.js

(function(exports) {
    'use strict';

    const MAX_EVENTS = 2_000_000; // cap parsed events to keep UI responsive

    /**
     * Parse a TOKIOTRC binary trace buffer
     * @param {ArrayBuffer} buffer - The binary trace data
     * @returns {Object} Parsed trace with events, metadata, and CPU samples
     */
    function parseTrace(buffer) {
        const view = new DataView(buffer);
        let off = 0;
        const magic = String.fromCharCode(
            ...new Uint8Array(buffer, 0, 8),
        );
        off += 8;
        const version = view.getUint32(off, true);
        off += 4;
        if (magic !== "TOKIOTRC")
            throw new Error("Not a TOKIOTRC file (got: " + magic + ")");
        if (version < 8 || version > 17) {
            console.warn(`Expected version 8-17, got ${version}. Some data may be missing.`);
        }
        const hasCpuTime = version >= 5;
        const hasSchedWait = version >= 6;
        const hasTaskTracking = version >= 7;

        const events = [];
        const spawnLocations = new Map(); // SpawnLocationId (number) → string
        const taskSpawnLocs = new Map();  // taskId (number) → SpawnLocationId (number)
        const taskSpawnTimes = new Map(); // taskId (number) → timestamp (nanoseconds)
        const taskTerminateTimes = new Map(); // taskId (number) → timestamp (nanoseconds)
        const callframeSymbols = new Map(); // address (bigint as string) → symbol name
        const cpuSamples = []; // {timestamp, workerId, tid, source, callchain: [addr strings]}
        const threadNames = new Map(); // tid (number) → thread name (string)
        const decoder = new TextDecoder();
        
        while (off < buffer.byteLength && events.length < MAX_EVENTS) {
            if (off + 1 > buffer.byteLength) break;
            const wireCode = view.getUint8(off);
            off += 1;

            // SpawnLocationDef and TaskSpawn have no timestamp — handle before reading ts
            if (wireCode === 5) {
                if (off + 4 > buffer.byteLength) break;
                const spawnLocId = view.getUint16(off, true); off += 2;
                const strLen = view.getUint16(off, true); off += 2;
                if (off + strLen > buffer.byteLength) break;
                spawnLocations.set(spawnLocId, decoder.decode(new Uint8Array(buffer, off, strLen)));
                off += strLen;
                continue;
            }
            if (wireCode === 6) {
                // TaskSpawn: timestamp_us(4) + task_id(4) + spawn_loc_id(2)
                if (off + 10 > buffer.byteLength) break;
                const timestampUs = view.getUint32(off, true); off += 4;
                const taskId = view.getUint32(off, true); off += 4;
                const spawnLocId = view.getUint16(off, true); off += 2;
                taskSpawnLocs.set(taskId, spawnLocId);
                taskSpawnTimes.set(taskId, timestampUs * 1000);
                continue;
            }
            if (wireCode === 7) {
                // WakeEvent: timestamp_us(4) + waker_task_id(4) + woken_task_id(4) + target_worker(1)
                if (off + 13 > buffer.byteLength) break;
                const timestampUs = view.getUint32(off, true); off += 4;
                const wakerTaskId = view.getUint32(off, true); off += 4;
                const wokenTaskId = view.getUint32(off, true); off += 4;
                const targetWorker = view.getUint8(off); off += 1;
                // Store wake event
                events.push({
                    eventType: 9, // WakeEvent
                    timestamp: timestampUs * 1000,
                    workerId: targetWorker,
                    wakerTaskId,
                    wokenTaskId,
                    targetWorker,
                    globalQueue: 0, localQueue: 0, cpuTime: 0, schedWait: 0,
                    taskId: 0, spawnLocId: 0, spawnLoc: null,
                });
                continue;
            }
            if (wireCode === 8) {
                // CpuSample: timestamp_us(4) + worker_id(1) + tid(4) + source(1) + num_frames(1) + frames(N*8)
                if (off + 11 > buffer.byteLength) break;
                const tsUs = view.getUint32(off, true); off += 4;
                const wid = view.getUint8(off); off += 1;
                const tid = view.getUint32(off, true); off += 4;
                const src = view.getUint8(off); off += 1; // 0=CpuProfile, 1=SchedEvent
                const nf = view.getUint8(off); off += 1;
                if (off + nf * 8 > buffer.byteLength) break;
                const chain = [];
                for (let i = 0; i < nf; i++) {
                    const lo = view.getUint32(off, true);
                    const hi = view.getUint32(off + 4, true);
                    off += 8;
                    chain.push("0x" + (hi * 0x100000000 + lo).toString(16));
                }
                cpuSamples.push({ timestamp: tsUs * 1000, workerId: wid, tid, source: src, callchain: chain });
                continue;
            }

            if (wireCode === 9) {
                // CallframeDef: address(8) + symbol_len(2) + symbol_bytes(N) + location_len(2) + location_bytes(M)
                if (off + 10 > buffer.byteLength) break;
                const lo = view.getUint32(off, true);
                const hi = view.getUint32(off + 4, true);
                off += 8;
                const addrKey = "0x" + (hi * 0x100000000 + lo).toString(16);
                
                // Read symbol
                const symbolLen = view.getUint16(off, true); off += 2;
                if (off + symbolLen > buffer.byteLength) break;
                const symbol = decoder.decode(new Uint8Array(buffer, off, symbolLen));
                off += symbolLen;
                
                // Read optional location
                if (off + 2 > buffer.byteLength) break;
                const locationLen = view.getUint16(off, true); off += 2;
                let location = null;
                if (locationLen !== 0xFFFF) {
                    if (off + locationLen > buffer.byteLength) break;
                    location = decoder.decode(new Uint8Array(buffer, off, locationLen));
                    off += locationLen;
                }
                
                // Store symbol and location separately
                callframeSymbols.set(addrKey, { symbol, location });
                continue;
            }

            if (wireCode === 10) {
                // ThreadNameDef: tid(4) + string_len(2) + string_bytes(N)
                if (off + 6 > buffer.byteLength) break;
                const tid = view.getUint32(off, true); off += 4;
                const strLen = view.getUint16(off, true); off += 2;
                if (off + strLen > buffer.byteLength) break;
                threadNames.set(tid, decoder.decode(new Uint8Array(buffer, off, strLen)));
                off += strLen;
                continue;
            }

            if (wireCode === 11) {
                // SegmentMetadata: num_entries(2) + (key_len(2) + key + val_len(2) + val)*
                if (off + 2 > buffer.byteLength) break;
                const numEntries = view.getUint16(off, true); off += 2;
                let truncated = false;
                for (let i = 0; i < numEntries; i++) {
                    if (off + 2 > buffer.byteLength) { truncated = true; break; }
                    const kLen = view.getUint16(off, true); off += 2;
                    if (off + kLen > buffer.byteLength) { truncated = true; break; }
                    off += kLen; // skip key
                    if (off + 2 > buffer.byteLength) { truncated = true; break; }
                    const vLen = view.getUint16(off, true); off += 2;
                    if (off + vLen > buffer.byteLength) { truncated = true; break; }
                    off += vLen; // skip value
                }
                if (truncated) break; // break outer loop
                // TODO(#68): Store metadata entries for display in the viewer (service name, host, etc.)
                continue;
            }

            if (wireCode === 172) {
                // TaskTerminate: timestamp_us(4) + task_id(4)
                if (off + 8 > buffer.byteLength) break;
                const timestampUs = view.getUint32(off, true); off += 4;
                const taskId = view.getUint32(off, true); off += 4;
                taskTerminateTimes.set(taskId, timestampUs * 1000);
                continue;
            }

            if (wireCode > 11 && wireCode !== 172) break; // unknown code

            // All regular codes have a 4-byte timestamp next
            if (off + 4 > buffer.byteLength) break;
            const timestampUs = view.getUint32(off, true);
            off += 4;
            const timestamp = timestampUs * 1000;

            let eventType,
                workerId = 0,
                globalQueue = 0,
                localQueue = 0,
                cpuTime = 0,
                schedWait = 0,
                taskId = 0,
                spawnLocId = 0;
            switch (wireCode) {
                case 0: // PollStart
                    if (hasTaskTracking) {
                        // v8: worker(1) + lq(1) + task_id(4) + spawn_loc_id(2) = 8
                        if (off + 8 > buffer.byteLength) break;
                        eventType = 0;
                        workerId = view.getUint8(off); off += 1;
                        localQueue = view.getUint8(off); off += 1;
                        taskId = view.getUint32(off, true); off += 4;
                        spawnLocId = view.getUint16(off, true); off += 2;
                    } else {
                        if (off + 1 > buffer.byteLength) break;
                        eventType = 0;
                        workerId = view.getUint8(off); off += 1;
                    }
                    break;
                case 1: // PollEnd
                    if (off + 1 > buffer.byteLength) break;
                    eventType = 1;
                    workerId = view.getUint8(off); off += 1;
                    break;
                case 2: { // WorkerPark
                    const need = hasCpuTime ? 6 : 2;
                    if (off + need > buffer.byteLength) break;
                    eventType = 2;
                    workerId = view.getUint8(off); off += 1;
                    localQueue = view.getUint8(off); off += 1;
                    if (hasCpuTime) {
                        cpuTime = view.getUint32(off, true) * 1000; off += 4;
                    }
                    break;
                }
                case 3: { // WorkerUnpark
                    const need = hasCpuTime ? (hasSchedWait ? 10 : 6) : 2;
                    if (off + need > buffer.byteLength) break;
                    eventType = 3;
                    workerId = view.getUint8(off); off += 1;
                    localQueue = view.getUint8(off); off += 1;
                    if (hasCpuTime) {
                        cpuTime = view.getUint32(off, true) * 1000; off += 4;
                    }
                    if (hasSchedWait) {
                        schedWait = view.getUint32(off, true); off += 4;
                    }
                    break;
                }
                case 4: // QueueSample
                    if (off + 1 > buffer.byteLength) break;
                    eventType = 4;
                    globalQueue = view.getUint8(off); off += 1;
                    break;
            }
            // Also build taskSpawnLocs from PollStart (covers tasks without TaskSpawn events)
            if (eventType === 0 && taskId && spawnLocId && !taskSpawnLocs.has(taskId)) {
                taskSpawnLocs.set(taskId, spawnLocId);
            }
            events.push({
                eventType, timestamp, workerId,
                globalQueue, localQueue, cpuTime, schedWait,
                taskId, spawnLocId,
                spawnLoc: spawnLocations.get(spawnLocId) ?? null,
            });
        }
        return { 
            magic, 
            version, 
            events, 
            truncated: events.length >= MAX_EVENTS, 
            hasCpuTime, 
            hasSchedWait, 
            hasTaskTracking, 
            spawnLocations, 
            taskSpawnLocs, 
            taskSpawnTimes,
            cpuSamples, 
            callframeSymbols,
            threadNames,
            taskTerminateTimes
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
            if (!callframeSymbols) {
                throw new Error('formatFrame requires callframeSymbols when given an address string');
            }
            const entry = callframeSymbols.get(frame);
            if (!entry) return { text: frame || '(unknown)', docsUrl: null };
            frame = entry;
        }
        const { symbol: sym, location } = frame;
        if (!sym || sym.startsWith('0x')) return { text: sym || '(unknown)', docsUrl: null };

        let result = sym;

        const traitImplMatch = result.match(/^<(.+?) as (.+?)>::(.+)$/);
        if (traitImplMatch) {
            let [, implType, trait_, method] = traitImplMatch;
            const shortType = _lastSeg(_stripBoringGenerics(implType));
            if (shortType.length <= 2) {
                result = `${_lastSeg(_stripBoringGenerics(trait_))}::${method}`;
            } else {
                result = `${shortType}::${method}`;
            }
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
     * @param {string[]} callchain - Address strings like "0x55cc6d053893"
     * @param {Map<string, {symbol: string, location: string|null}>} callframeSymbols
     * @returns {{symbol: string, location: string|null}[]}
     */
    function symbolizeChain(callchain, callframeSymbols) {
        return callchain.map(addr => {
            const entry = callframeSymbols.get(addr);
            if (!entry) return { symbol: addr, location: null };
            if (typeof entry === 'string') return { symbol: entry, location: null };
            return entry;
        });
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
                    count: 0,
                    frames,
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
