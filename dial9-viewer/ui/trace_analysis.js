// trace_analysis.js - Derived analysis on parsed trace data
// Can be used in browser or Node.js

(function (exports) {
  "use strict";

  function getParser() {
    if (typeof require !== "undefined") {
      return require("./trace_parser.js");
    }
    if (typeof TraceParser !== "undefined") return TraceParser;
    throw new Error(
      "TraceParser not found. Load trace_parser.js before trace_analysis.js"
    );
  }

  const parser = getParser();
  const EVENT_TYPES = parser.EVENT_TYPES;
  const formatFrame = parser.formatFrame;

  /**
   * Reconstruct poll/park/active spans from raw events using a state machine.
   * @param {import('./trace_parser.js').TraceEvent[]} events - raw trace events
   * @param {number[]} workerIds - sorted worker IDs
   * @param {number} maxTs - end-of-trace timestamp for closing open spans
   * @returns {{
   *   workerSpans: Object<number, { polls: Array<{start: number, end: number, taskId?: number, spawnLocId?: string|null, spawnLoc?: string|null}>, parks: Array<{start: number, end: number, schedWait: number}>, actives: Array<{start: number, end: number, ratio: number}>, cpuSampleTimes: number[] }>,
   *   perWorker: Object<number, import('./trace_parser.js').TraceEvent[]>,
   *   queueSamples: Array<{t: number, global: number}>,
   *   workerQueueSamples: Object<number, Array<{t: number, local: number}>>,
   *   maxLocalQueue: number,
   *   wakesByTask: Object<number, Array<{timestamp: number, wakerTaskId: number, targetWorker: number}>>,
   *   wakesByWorker: Object<number, Array<{timestamp: number, wakerTaskId: number, wokenTaskId: number}>>,
   * }}
   */
  function buildWorkerSpans(events, workerIds, maxTs) {
    const workerSpans = {};
    const openPoll = {},
      openPark = {},
      openUnpark = {};
    const openPollMeta = {};
    const workerQueueSamples = {};
    let maxLocalQueue = 1;
    const wakesByTask = {};
    const wakesByWorker = {};
    for (const w of workerIds) {
      workerSpans[w] = {
        polls: [],
        parks: [],
        actives: [],
        cpuSampleTimes: [],
      };
      workerQueueSamples[w] = [];
    }

    // Group events by worker and sort per-worker by timestamp
    // Also index wake events in the same pass
    const perWorker = {};
    for (const e of events) {
      if (e.eventType === EVENT_TYPES.WakeEvent) {
        (wakesByTask[e.wokenTaskId] ??= []).push({
          timestamp: e.timestamp,
          wakerTaskId: e.wakerTaskId,
          targetWorker: e.targetWorker,
        });
        (wakesByWorker[e.targetWorker] ??= []).push({
          timestamp: e.timestamp,
          wakerTaskId: e.wakerTaskId,
          wokenTaskId: e.wokenTaskId,
        });
      } else if (e.eventType !== EVENT_TYPES.QueueSample) {
        (perWorker[e.workerId] ??= []).push(e);
      }
    }
    for (const wEvents of Object.values(perWorker)) {
      wEvents.sort((a, b) => a.timestamp - b.timestamp);
    }
    for (const arr of Object.values(wakesByTask)) {
      arr.sort((a, b) => a.timestamp - b.timestamp);
    }
    for (const arr of Object.values(wakesByWorker)) {
      arr.sort((a, b) => a.timestamp - b.timestamp);
    }

    for (const [w, wEvents] of Object.entries(perWorker)) {
      for (const e of wEvents) {
        // Extract local queue samples inline
        if (
          e.eventType === EVENT_TYPES.PollStart ||
          e.eventType === EVENT_TYPES.WorkerPark ||
          e.eventType === EVENT_TYPES.WorkerUnpark
        ) {
          workerQueueSamples[w].push({ t: e.timestamp, local: e.localQueue });
          if (e.localQueue > maxLocalQueue) maxLocalQueue = e.localQueue;
        }

        if (e.eventType === EVENT_TYPES.PollStart) {
          openPoll[w] = e.timestamp;
          openPollMeta[w] = {
            taskId: e.taskId,
            spawnLocId: e.spawnLocId,
            spawnLoc: e.spawnLoc,
          };
        } else if (e.eventType === EVENT_TYPES.PollEnd) {
          if (openPoll[w] != null) {
            const meta = openPollMeta[w] || {
              taskId: 0,
              spawnLocId: 0,
              spawnLoc: null,
            };
            workerSpans[w].polls.push({
              start: openPoll[w],
              end: e.timestamp,
              taskId: meta.taskId,
              spawnLocId: meta.spawnLocId,
              spawnLoc: meta.spawnLoc,
            });
            openPoll[w] = null;
          }
        } else if (e.eventType === EVENT_TYPES.WorkerPark) {
          openPark[w] = e.timestamp;
          if (openUnpark[w] != null) {
            const wallDelta = e.timestamp - openUnpark[w].timestamp;
            const cpuDelta = e.cpuTime - openUnpark[w].cpuTime;
            const ratio =
              wallDelta > 0 ? Math.min(cpuDelta / wallDelta, 1.0) : 1.0;
            workerSpans[w].actives.push({
              start: openUnpark[w].timestamp,
              end: e.timestamp,
              ratio,
            });
            openUnpark[w] = null;
          }
        } else if (e.eventType === EVENT_TYPES.WorkerUnpark) {
          if (openPark[w] != null) {
            workerSpans[w].parks.push({
              start: openPark[w],
              end: e.timestamp,
              schedWait: e.schedWait,
            });
            openPark[w] = null;
          }
          openUnpark[w] = { timestamp: e.timestamp, cpuTime: e.cpuTime };
        }
      }
    }

    // Close any open park spans at trace end.
    // Open polls are discarded: a PollStart without a matching PollEnd
    // means the segment rotated mid-poll, not that the poll was long (#194).
    for (const w of workerIds) {
      if (openPark[w] != null)
        workerSpans[w].parks.push({ start: openPark[w], end: maxTs });
    }

    // Global queue samples
    const queueSamples = events
      .filter((e) => e.eventType === EVENT_TYPES.QueueSample)
      .map((e) => ({ t: e.timestamp, global: e.globalQueue }));

    return { workerSpans, perWorker, queueSamples, workerQueueSamples, maxLocalQueue, wakesByTask, wakesByWorker };
  }

  /**
   * Attach CPU samples to the poll spans they fall within using binary search.
   * Mutates workerSpans poll objects (adds .cpuSamples[], .schedSamples[])
   * and sample objects (sets .spawnLoc).
   * @param {import('./trace_parser.js').CpuSample[]} cpuSamples
   * @param {Object} workerSpans - as returned by buildWorkerSpans
   * @returns {{ pollsWithCpuSamples: number, pollsWithSchedSamples: number }}
   */
  function attachCpuSamples(cpuSamples, workerSpans) {
    for (const sample of cpuSamples) {
      const spans = workerSpans[sample.workerId];
      if (!spans) {
        sample.spawnLoc = null;
        continue;
      }
      if (sample.source !== 1) spans.cpuSampleTimes.push(sample.timestamp);
      const polls = spans.polls;
      const ts = sample.timestamp;
      let lo = 0,
        hi = polls.length - 1,
        found = false;
      while (lo <= hi) {
        const mid = (lo + hi) >> 1;
        if (polls[mid].start <= ts) {
          lo = mid + 1;
        } else {
          hi = mid - 1;
        }
      }
      if (hi >= 0 && ts <= polls[hi].end) {
        const poll = polls[hi];
        if (sample.source === 1) {
          (poll.schedSamples ??= []).push(sample);
        } else {
          (poll.cpuSamples ??= []).push(sample);
        }
        sample.spawnLoc = poll.spawnLoc;
        found = true;
      }
      if (!found) sample.spawnLoc = null;
    }

    let pollsWithCpuSamples = 0;
    let pollsWithSchedSamples = 0;
    for (const w of Object.keys(workerSpans)) {
      for (const p of workerSpans[w].polls) {
        if (p.cpuSamples) pollsWithCpuSamples++;
        if (p.schedSamples) pollsWithSchedSamples++;
      }
    }
    return { pollsWithCpuSamples, pollsWithSchedSamples };
  }

  /**
   * Build active task count timeline from spawn/terminate timestamps.
   * @param {Map<number, number>} taskSpawnTimes
   * @param {Map<number, number>} taskTerminateTimes
   * @returns {{ activeTaskSamples: Array<{t: number, count: number}>, taskFirstPoll: Map<number, number> }}
   */
  function buildActiveTaskTimeline(taskSpawnTimes, taskTerminateTimes) {
    const activeTaskSamples = [];
    const taskFirstPoll = new Map();
    if (taskSpawnTimes && taskSpawnTimes.size > 0) {
      const taskEvents = [];
      for (const [taskId, t] of taskSpawnTimes) {
        taskFirstPoll.set(taskId, t);
        taskEvents.push({ t, delta: 1 });
      }
      for (const [, t] of taskTerminateTimes) {
        taskEvents.push({ t, delta: -1 });
      }
      taskEvents.sort((a, b) => a.t - b.t);
      let count = 0;
      for (const te of taskEvents) {
        count += te.delta;
        activeTaskSamples.push({ t: te.t, count: Math.max(0, count) });
      }
    }
    return { activeTaskSamples, taskFirstPoll };
  }

  /**
   * Compute scheduling delays: for each poll, find the most recent wake before it.
   * Adjusts for mid-poll wake arrivals.
   * @param {Object} workerSpans - as returned by buildWorkerSpans
   * @param {number[]} workerIds
   * @param {Object} wakesByTask - as returned by buildWorkerSpans
   * @returns {Array<{wakeTime: number, pollTime: number, delay: number, taskId: number, wakerTaskId: number, worker: number, poll: Object}>}
   */
  function computeSchedulingDelays(workerSpans, workerIds, wakesByTask) {
    const pollsByTask = {};
    for (const w of workerIds) {
      for (const s of workerSpans[w].polls) {
        if (s.taskId) (pollsByTask[s.taskId] ??= []).push(s);
      }
    }
    for (const arr of Object.values(pollsByTask)) {
      arr.sort((a, b) => a.start - b.start);
    }

    const schedDelays = [];
    for (const w of workerIds) {
      for (const s of workerSpans[w].polls) {
        if (!s.taskId) continue;
        const wakes = wakesByTask[s.taskId];
        if (!wakes || !wakes.length) continue;
        let lo = 0,
          hi = wakes.length - 1,
          best = -1;
        while (lo <= hi) {
          const mid = (lo + hi) >> 1;
          if (wakes[mid].timestamp <= s.start) {
            best = mid;
            lo = mid + 1;
          } else hi = mid - 1;
        }
        if (best >= 0) {
          const wake = wakes[best];
          let effectiveWake = wake.timestamp;
          const taskPolls = pollsByTask[s.taskId];
          if (taskPolls) {
            for (const p of taskPolls) {
              if (p.start >= s.start) break;
              if (wake.timestamp >= p.start && wake.timestamp <= p.end) {
                effectiveWake = p.end;
                break;
              }
            }
          }
          const delay = s.start - effectiveWake;
          if (delay > 0 && delay < 1e9) {
            schedDelays.push({
              wakeTime: effectiveWake,
              pollTime: s.start,
              delay,
              taskId: s.taskId,
              wakerTaskId: wake.wakerTaskId,
              worker: w,
              poll: s,
            });
          }
        }
      }
    }
    schedDelays.sort((a, b) => a.wakeTime - b.wakeTime);
    return schedDelays;
  }

  /**
   * Filter and sort points of interest from worker spans and scheduling delays.
   * @param {string} filterType - "sched" | "long-poll" | "cpu-sampled" | "wake-delay"
   * @param {Object} workerSpans
   * @param {number[]} workerIds
   * @param {Array} schedDelays - as returned by computeSchedulingDelays
   * @param {boolean} hasSchedWait
   * @param {{ sortByWorst?: boolean }} opts
   * @returns {Array<{time: number, worker: number, type: string, value: number, span: Object, schedDelay?: Object}>}
   */
  function filterPointsOfInterest(
    filterType,
    workerSpans,
    workerIds,
    schedDelays,
    opts
  ) {
    const hasSchedWait = opts && opts.hasSchedWait;
    const points = [];

    for (const w of workerIds) {
      const spans = workerSpans[w];

      if (filterType === "sched") {
        for (const s of spans.parks) {
          if (hasSchedWait && s.schedWait > 100) {
            const schedWaitNs = s.schedWait * 1000;
            const wakeupShouldBe = s.end - schedWaitNs;
            points.push({
              time: wakeupShouldBe,
              worker: w,
              type: "sched",
              value: s.schedWait,
              span: s,
            });
          }
        }
      } else if (filterType === "long-poll") {
        for (const s of spans.polls) {
          const durMs = (s.end - s.start) / 1e6;
          if (durMs > 1) {
            points.push({
              time: s.start,
              worker: w,
              type: "long-poll",
              value: durMs,
              span: s,
            });
          }
        }
      } else if (filterType === "cpu-sampled") {
        for (const s of spans.polls) {
          const cpuCount = s.cpuSamples ? s.cpuSamples.length : 0;
          const schedCount = s.schedSamples ? s.schedSamples.length : 0;
          if (cpuCount + schedCount > 0) {
            const durMs = (s.end - s.start) / 1e6;
            points.push({
              time: s.start,
              worker: w,
              type: "cpu-sampled",
              value: durMs,
              span: s,
            });
          }
        }
      }
    }

    if (filterType === "wake-delay") {
      for (const sd of schedDelays) {
        const delayUs = sd.delay / 1000;
        if (delayUs > 100) {
          points.push({
            time: sd.wakeTime,
            worker: sd.worker,
            type: "wake-delay",
            value: delayUs,
            span: sd.poll,
            schedDelay: sd,
          });
        }
      }
    }

    if (filterType === "uninstrumented" && opts && opts.taskInstrumented) {
      for (const w of workerIds) {
        for (const s of workerSpans[w].polls) {
          if (s.taskId && opts.taskInstrumented.get(s.taskId) === false) {
            points.push({
              time: s.start,
              worker: w,
              type: "uninstrumented",
              value: (s.end - s.start) / 1e6,
              span: s,
            });
          }
        }
      }
    }

    if (opts && opts.sortByWorst) {
      points.sort((a, b) => b.value - a.value);
    } else {
      points.sort((a, b) => a.time - b.time);
    }
    return points;
  }

  /**
   * Build a flamegraph tree from CPU samples with reversed callchains.
   * @param {import('./trace_parser.js').CpuSample[]} samples
   * @param {Map} callframeSymbols
   * @returns {{ name: string, children: Map, count: number, self: number }}
   */
  function buildFlamegraphTree(samples, callframeSymbols) {
    const root = { name: "(all)", children: new Map(), count: 0, self: 0 };
    for (const s of samples) {
      const chain = s.callchain.slice().reverse();
      let node = root;
      node.count++;
      for (const addr of chain) {
        const entry = callframeSymbols.get(addr);
        const resolved = Array.isArray(entry) ? entry[0] : entry;
        const key = resolved ? resolved.symbol : addr || "??";
        const formatted = formatFrame(addr, callframeSymbols);
        if (!node.children.has(key)) {
          node.children.set(key, {
            name: formatted.text,
            fullName: key,
            location: resolved ? resolved.location : null,
            docsUrl: formatted.docsUrl,
            children: new Map(),
            count: 0,
            self: 0,
          });
        }
        node = node.children.get(key);
        node.count++;
      }
      node.self++;
    }
    return root;
  }

  /**
   * Flatten a flamegraph tree into drawable nodes, filtering out nodes < 0.1%.
   * @param {{ name: string, children: Map, count: number, self: number }} root
   * @param {number} total - total sample count
   * @returns {{ nodes: Array<{name: string, depth: number, x: number, w: number, count: number, self: number}>, maxDepth: number }}
   */
  function flattenFlamegraph(root, total) {
    const nodes = [];
    let maxD = 0;
    function walk(node, depth, xStart) {
      const w = node.count / total;
      if (w < 0.001) return;
      nodes.push({
        name: node.name,
        depth,
        x: xStart,
        w,
        count: node.count,
        self: node.self,
      });
      if (depth > maxD) maxD = depth;
      const kids = [...node.children.values()].sort(
        (a, b) => b.count - a.count
      );
      let cx = xStart;
      for (const child of kids) {
        walk(child, depth + 1, cx);
        cx += child.count / total;
      }
    }
    const kids = [...root.children.values()].sort(
      (a, b) => b.count - a.count
    );
    let cx = 0;
    for (const child of kids) {
      walk(child, 0, cx);
      cx += child.count / total;
    }
    return { nodes, maxDepth: maxD };
  }

  /**
   * Build flamegraph data from samples (convenience wrapper).
   * @param {import('./trace_parser.js').CpuSample[]} samples
   * @param {Map} callframeSymbols
   * @returns {{ nodes: Array, maxDepth: number, totalSamples: number } | null}
   */
  function buildFgData(samples, callframeSymbols) {
    if (!samples.length) return null;
    const tree = buildFlamegraphTree(samples, callframeSymbols);
    const result = flattenFlamegraph(tree, samples.length);
    return {
      nodes: result.nodes,
      maxDepth: result.maxDepth,
      totalSamples: samples.length,
    };
  }

  /**
   * Build span data structures from custom events.
   * Groups SpanEnter/SpanExit pairs into spans with segments (one per poll).
   * SpanCloseEvent finalizes a span and enables span ID recycling.
   * @param {Array<{name: string, timestamp: number, fields: Object}>} customEvents
   * @returns {{
   *   allSpans: Array<{start: number, end: number, spanId: string, spanName: string, fields: Object, parentSpanId: string|null, segments: Array<{start: number, end: number, workerId: number}>, activeNs: number, depth: number}>,
   *   spanMeta: Map<string, {spanName: string, fields: Object, parentSpanId: string|null}>,
   *   maxDepth: number,
   *   unmatchedSpans: Array<{start: number, spanId: string, workerId: number, spanName: string, fields: Object, parentSpanId: string|null}>,
   *   childrenByParent: Map<string|null, string[]>,
   * }}
   */
  function buildSpanData(customEvents) {
    // Events are only ordered within a single worker's stream. Cross-worker
    // interleaving can produce globally out-of-order timestamps, so we must
    // sort before processing to ensure close events are seen after all
    // enter/exit pairs that precede them in wall-clock time.
    customEvents = [...customEvents].sort((a, b) => a.timestamp - b.timestamp);
    // Key by span_id only — a span may be polled on different workers.
    const openEnters = new Map(); // spanId → {timestamp, workerId}
    // Live span records keyed by spanId. Moved to closedSpans on SpanClose.
    const spanMap = new Map(); // spanId → {spanName, fields, parentSpanId, segments}
    const closedSpans = []; // finalized span records (after SpanClose or end-of-trace)
    const spanMeta = new Map();

    const BASE_ENTER_FIELDS = new Set(["worker_id", "span_id", "parent_span_id", "span_name"]);
    const BASE_EXIT_FIELDS = new Set(["worker_id", "span_id", "span_name"]);

    function finalizeSpan(spanId) {
      const rec = spanMap.get(spanId);
      if (rec && rec.segments.length > 0) {
        closedSpans.push({ spanId, ...rec });
      }
      spanMap.delete(spanId);
    }

    for (const ev of customEvents) {
      if (ev.name.startsWith("SpanEnter:") || ev.name === "SpanEnterEvent") {
        const v = ev.fields;
        const workerId = Number(v.worker_id);
        const spanId = String(v.span_id);
        const parentSpanId = v.parent_span_id != null ? String(v.parent_span_id) : null;
        const spanName = v.span_name || "unknown";
        const fields = {};
        for (const [k, val] of Object.entries(v)) {
          if (!BASE_ENTER_FIELDS.has(k)) fields[k] = val;
        }

        // Guard: if this span already has an open enter (e.g. entered on a
        // different worker before exiting), skip to avoid losing the first enter.
        if (openEnters.has(spanId)) continue;

        openEnters.set(spanId, { timestamp: ev.timestamp, workerId });

        if (!spanMap.has(spanId)) {
          spanMap.set(spanId, { spanName, fields, parentSpanId, segments: [] });
        }
        spanMeta.set(spanId, { spanName, fields, parentSpanId });
      } else if (ev.name.startsWith("SpanExit:") || ev.name === "SpanExitEvent") {
        const v = ev.fields;
        const workerId = Number(v.worker_id);
        const spanId = String(v.span_id);

        const enter = openEnters.get(spanId);
        if (enter) {
          openEnters.delete(spanId);
          const exitFields = {};
          for (const [k, val] of Object.entries(v)) {
            if (!BASE_EXIT_FIELDS.has(k)) exitFields[k] = val;
          }
          let rec = spanMap.get(spanId);
          if (!rec) {
            rec = { spanName: v.span_name || "unknown", fields: {}, parentSpanId: null, segments: [] };
            spanMap.set(spanId, rec);
          }
          if (Object.keys(exitFields).length > 0) rec.fields = exitFields;
          rec.segments.push({ start: enter.timestamp, end: ev.timestamp, workerId });
        }
      } else if (ev.name === "SpanCloseEvent") {
        const spanId = String(ev.fields.span_id);
        openEnters.delete(spanId);
        finalizeSpan(spanId);
      }
    }

    // Finalize any spans still open at end of trace (no SpanClose seen)
    for (const [spanId] of spanMap) {
      finalizeSpan(spanId);
    }

    // Build allSpans
    const allSpans = [];
    for (const rec of closedSpans) {
      rec.segments.sort((a, b) => a.start - b.start);
      const start = rec.segments[0].start;
      const end = rec.segments[rec.segments.length - 1].end;
      const activeNs = rec.segments.reduce((sum, seg) => sum + (seg.end - seg.start), 0);
      allSpans.push({
        start, end,
        spanId: rec.spanId,
        spanName: rec.spanName,
        fields: rec.fields,
        parentSpanId: rec.parentSpanId,
        segments: rec.segments,
        activeNs,
      });
    }
    allSpans.sort((a, b) => a.start - b.start);

    // Unmatched: open enters with no segments
    const unmatchedSpans = [];
    for (const [spanId, enter] of openEnters) {
      unmatchedSpans.push({
        start: enter.timestamp,
        spanId,
        workerId: enter.workerId,
        spanName: spanMeta.get(spanId)?.spanName || "unknown",
        fields: spanMeta.get(spanId)?.fields || {},
        parentSpanId: spanMeta.get(spanId)?.parentSpanId ?? null,
      });
    }
    unmatchedSpans.sort((a, b) => a.start - b.start);

    // Compute depth via parent chain
    const depthCache = new Map();
    function getDepth(spanId, seen) {
      if (spanId == null) return -1;
      if (depthCache.has(spanId)) return depthCache.get(spanId);
      if (seen && seen.has(spanId)) { depthCache.set(spanId, 0); return 0; }
      const meta = spanMeta.get(spanId);
      if (!meta) { depthCache.set(spanId, 0); return 0; }
      const visited = seen || new Set();
      visited.add(spanId);
      const d = getDepth(meta.parentSpanId, visited) + 1;
      depthCache.set(spanId, d);
      return d;
    }
    let maxDepth = 0;
    for (const s of allSpans) {
      s.depth = getDepth(s.spanId);
      if (s.depth > maxDepth) maxDepth = s.depth;
    }

    // Build parent → children index.  Roots (parent == null) are stored under the null key.
    // Every closed span contributes exactly one entry to its parent's bucket; childless
    // spans have no bucket at all (callers must treat a missing key as empty).
    const childrenByParent = new Map();
    const addChild = (parentKey, childId) => {
      let arr = childrenByParent.get(parentKey);
      if (!arr) { arr = []; childrenByParent.set(parentKey, arr); }
      arr.push(childId);
    };
    for (const s of allSpans) {
      addChild(s.parentSpanId ?? null, s.spanId);
    }

    return { allSpans, spanMeta, maxDepth, unmatchedSpans, childrenByParent };
  }

  /**
   * Collect a set of span IDs containing the given seeds plus all their descendants.
   * Cycle-safe.
   * @param {string[]} seedIds
   * @param {Map<string|null, string[]>} childrenByParent
   * @returns {Set<string>}
   */
  function collectDescendants(seedIds, childrenByParent) {
    const result = new Set();
    const stack = [...seedIds];
    while (stack.length > 0) {
      const id = stack.pop();
      if (result.has(id)) continue;
      result.add(id);
      const children = childrenByParent.get(id);
      if (children) {
        for (const c of children) stack.push(c);
      }
    }
    return result;
  }

  /**
   * Select which spans to render based on focus state.
   * - No focus: return only root-like spans (parentSpanId is null or parent not in allSpans).
   * - Focused: return the focused span + all its descendants.
   * @param {{ allSpans: Array, focusedSpanId: string|null, childrenByParent: Map }} opts
   * @returns {Array}
   */
  function selectSpanRenderSet({ allSpans, focusedSpanId, childrenByParent }) {
    if (focusedSpanId != null) {
      const ids = collectDescendants([focusedSpanId], childrenByParent);
      return allSpans.filter(s => ids.has(s.spanId));
    }
    // Root view: spans whose parent is null or whose parent is not in the dataset
    const allIds = new Set(allSpans.map(s => s.spanId));
    return allSpans.filter(s => s.parentSpanId == null || !allIds.has(s.parentSpanId));
  }

  /**
   * Compute span panel layout with duration-based y and pixel-grid clustering.
   * @param {{ spans: Array, viewStart: number, viewEnd: number, drawW: number, panelH: number, clusterXPx: number, barH: number }} opts
   * @returns {{ buckets: Array<{spans: Array, representative: Object, x1: number, x2: number, y: number, h: number}> }}
   */
  function computeSpanLayout({ spans, viewStart, viewEnd, drawW, panelH, clusterXPx, barH }) {
    if (spans.length === 0) return { buckets: [], minDur: 0, maxDur: 0 };
    if (viewEnd === viewStart) return { buckets: [], minDur: 0, maxDur: 0 };

    const PAD_TOP = 2;
    const PAD_BOT = 2;
    const usableH = panelH - PAD_TOP - PAD_BOT - barH;

    // Compute duration for each span and find min/max log-duration
    const durations = spans.map(s => s.end - s.start);
    let minLog = Infinity, maxLog = -Infinity;
    const logs = durations.map(d => {
      const l = Math.log(Math.max(d, 1));
      if (l < minLog) minLog = l;
      if (l > maxLog) maxLog = l;
      return l;
    });
    const logRange = maxLog - minLog || 1;

    const nsToX = (ns) => ((ns - viewStart) / (viewEnd - viewStart)) * drawW;

    // Assign each span a y based on log-duration (longer → smaller y → higher)
    // and an x midpoint, then bucket by pixel grid.
    const grid = new Map(); // "cellX,cellY" → {spans[], bestIdx}
    for (let i = 0; i < spans.length; i++) {
      const s = spans[i];
      const normDur = (logs[i] - minLog) / logRange; // 0 = shortest, 1 = longest
      const y = PAD_TOP + (1 - normDur) * usableH;
      const xMid = nsToX((s.start + s.end) / 2);

      const cellX = Math.floor(xMid / clusterXPx);
      const cellY = Math.floor(y / (barH + 1));
      const key = cellX + "," + cellY;

      let cell = grid.get(key);
      if (!cell) {
        cell = { spans: [], bestIdx: i, y, xMin: xMid, xMax: xMid };
        grid.set(key, cell);
      }
      cell.spans.push(s);
      // Track representative as the longest span
      if (durations[i] > durations[cell.bestIdx]) cell.bestIdx = i;
      // Track x extent for drawing
      const x1 = nsToX(s.start);
      const x2 = nsToX(s.end);
      if (x1 < cell.xMin) cell.xMin = x1;
      if (x2 > cell.xMax) cell.xMax = x2;
    }

    // Convert grid cells to buckets
    const buckets = [];
    for (const cell of grid.values()) {
      const rep = spans[cell.bestIdx] || cell.spans[0];
      const repX1 = Math.max(0, nsToX(rep.start));
      const repX2 = Math.min(drawW, nsToX(rep.end));
      buckets.push({
        spans: cell.spans,
        representative: rep,
        x1: repX1,
        x2: repX2,
        y: cell.y,
        h: barH,
      });
    }

    return { buckets, minDur: Math.exp(minLog), maxDur: Math.exp(maxLog) };
  }

  // Export for both browser and Node.js
  const analysisExports = {
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
  };

  if (typeof module !== "undefined" && module.exports) {
    module.exports = analysisExports;
  } else {
    exports.TraceAnalysis = analysisExports;
  }
})(typeof exports === "undefined" ? this : exports);
