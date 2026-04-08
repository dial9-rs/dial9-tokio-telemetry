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
   * }}
   */
  function buildWorkerSpans(events, workerIds, maxTs) {
    const workerSpans = {};
    const openPoll = {},
      openPark = {},
      openUnpark = {};
    const openPollMeta = {};
    for (const w of workerIds) {
      workerSpans[w] = {
        polls: [],
        parks: [],
        actives: [],
        cpuSampleTimes: [],
      };
    }

    // Group events by worker and sort per-worker by timestamp
    const perWorker = {};
    for (const e of events) {
      if (
        e.eventType !== EVENT_TYPES.QueueSample &&
        e.eventType !== EVENT_TYPES.WakeEvent
      ) {
        (perWorker[e.workerId] ??= []).push(e);
      }
    }
    for (const wEvents of Object.values(perWorker)) {
      wEvents.sort((a, b) => a.timestamp - b.timestamp);
    }

    for (const [w, wEvents] of Object.entries(perWorker)) {
      for (const e of wEvents) {
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

    // Close any open spans at trace end
    for (const w of workerIds) {
      if (openPoll[w] != null)
        workerSpans[w].polls.push({ start: openPoll[w], end: maxTs });
      if (openPark[w] != null)
        workerSpans[w].parks.push({ start: openPark[w], end: maxTs });
    }

    // Global queue samples
    const queueSamples = events
      .filter((e) => e.eventType === EVENT_TYPES.QueueSample)
      .map((e) => ({ t: e.timestamp, global: e.globalQueue }));

    return { workerSpans, perWorker, queueSamples };
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
   * Extract per-worker local queue depth samples from sorted per-worker events.
   * @param {Object<number, import('./trace_parser.js').TraceEvent[]>} perWorker
   * @param {number[]} workerIds
   * @returns {{ workerQueueSamples: Object<number, Array<{t: number, local: number}>>, maxLocalQueue: number }}
   */
  function extractLocalQueueSamples(perWorker, workerIds) {
    const workerQueueSamples = {};
    let maxLocalQueue = 1;
    for (const w of workerIds) {
      workerQueueSamples[w] = [];
    }
    for (const [w, wEvents] of Object.entries(perWorker)) {
      for (const e of wEvents) {
        if (
          e.eventType === EVENT_TYPES.PollStart ||
          e.eventType === EVENT_TYPES.WorkerPark ||
          e.eventType === EVENT_TYPES.WorkerUnpark
        ) {
          workerQueueSamples[w] ??= [];
          workerQueueSamples[w].push({ t: e.timestamp, local: e.localQueue });
          if (e.localQueue > maxLocalQueue) maxLocalQueue = e.localQueue;
        }
      }
    }
    return { workerQueueSamples, maxLocalQueue };
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
   * Index wake events by woken task ID and by target worker.
   * @param {import('./trace_parser.js').TraceEvent[]} events
   * @returns {{
   *   wakesByTask: Object<number, Array<{timestamp: number, wakerTaskId: number, targetWorker: number}>>,
   *   wakesByWorker: Object<number, Array<{timestamp: number, wakerTaskId: number, wokenTaskId: number}>>,
   * }}
   */
  function indexWakeEvents(events) {
    const wakesByTask = {};
    const wakesByWorker = {};
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
      }
    }
    for (const arr of Object.values(wakesByTask)) {
      arr.sort((a, b) => a.timestamp - b.timestamp);
    }
    for (const arr of Object.values(wakesByWorker)) {
      arr.sort((a, b) => a.timestamp - b.timestamp);
    }
    return { wakesByTask, wakesByWorker };
  }

  /**
   * Compute scheduling delays: for each poll, find the most recent wake before it.
   * Adjusts for mid-poll wake arrivals.
   * @param {Object} workerSpans - as returned by buildWorkerSpans
   * @param {number[]} workerIds
   * @param {Object} wakesByTask - as returned by indexWakeEvents
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
    hasSchedWait,
    opts
  ) {
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
            points.push({
              time: s.start,
              worker: w,
              type: "cpu-sampled",
              value: cpuCount + schedCount,
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
        const key = entry ? entry.symbol : addr || "??";
        const name = formatFrame(addr, callframeSymbols).text;
        if (!node.children.has(key)) {
          node.children.set(key, {
            name,
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

  // Export for both browser and Node.js
  const analysisExports = {
    buildWorkerSpans,
    attachCpuSamples,
    extractLocalQueueSamples,
    buildActiveTaskTimeline,
    indexWakeEvents,
    computeSchedulingDelays,
    filterPointsOfInterest,
    buildFlamegraphTree,
    flattenFlamegraph,
    buildFgData,
  };

  if (typeof module !== "undefined" && module.exports) {
    module.exports = analysisExports;
  } else {
    exports.TraceAnalysis = analysisExports;
  }
})(typeof exports === "undefined" ? this : exports);
