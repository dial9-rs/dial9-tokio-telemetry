// flamegraph.js - Shared flamegraph rendering with zoom and search

(function (exports) {
  "use strict";

  function getAnalysis() {
    if (typeof require !== "undefined") return require("./trace_analysis.js");
    if (typeof TraceAnalysis !== "undefined") return TraceAnalysis;
    throw new Error(
      "TraceAnalysis not found. Load trace_analysis.js before flamegraph.js"
    );
  }

  const buildFlamegraphTree = getAnalysis().buildFlamegraphTree;
  const FG_ROW_H = 18;

  function flamegraphColor(name) {
    let h = 0;
    for (let i = 0; i < name.length; i++) h = (h * 31 + name.charCodeAt(i)) | 0;
    const hue = 10 + (Math.abs(h) % 40);
    const sat = 60 + (Math.abs(h >> 8) % 30);
    const lit = 40 + (Math.abs(h >> 16) % 15);
    return `hsl(${hue},${sat}%,${lit}%)`;
  }

  // Like flattenFlamegraph in trace_analysis.js but attaches treeNode refs
  // for click-to-zoom. Filters out nodes < 0.1% of total.
  function flattenFromNode(root, total, includeRoot) {
    const nodes = [];
    let maxD = 0;
    const startDepth = includeRoot ? 1 : 0;
    if (includeRoot) {
      nodes.push({
        name: root.name, depth: 0, x: 0, w: 1,
        count: root.count, self: root.self, treeNode: root,
      });
    }
    function walk(treeNode, depth, xStart) {
      const w = treeNode.count / total;
      if (w < 0.001) return;
      nodes.push({
        name: treeNode.name,
        depth,
        x: xStart,
        w,
        count: treeNode.count,
        self: treeNode.self,
        treeNode,
      });
      if (depth > maxD) maxD = depth;
      const kids = [...treeNode.children.values()].sort(
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
      walk(child, startDepth, cx);
      cx += child.count / total;
    }
    return { nodes, maxDepth: maxD };
  }

  // Count matching frames and their self-samples for search stats.
  function countSearchMatches(root, queryLower) {
    let selfCount = 0;
    let frameCount = 0;
    function walk(node) {
      if (node.name.toLowerCase().includes(queryLower)) {
        selfCount += node.self;
        frameCount++;
      }
      for (const child of node.children.values()) walk(child);
    }
    walk(root);
    return { selfCount, frameCount };
  }

  function filterCpuSamples(cpuSamples, startNs, endNs) {
    let out = cpuSamples.filter((s) => s.callchain.length > 0 && s.source !== 1);
    if (startNs != null) out = out.filter((s) => s.timestamp >= startNs);
    if (endNs != null) out = out.filter((s) => s.timestamp <= endNs);
    return out;
  }

  function createFlamegraph(container, onZoomChange) {
    onZoomChange = onZoomChange || function () {};
    let workerTree = null;
    let offworkerTree = null;
    let workerData = null;
    let offworkerData = null;
    let workerZoomStack = [];
    let offworkerZoomStack = [];
    let searchQuery = "";
    let highlightName = null;
    let repaintQueued = false;
    let allSamples = [];
    let currentSymbols = null;
    const hitRegions = { worker: [], offworker: [] };

    // DOM
    const searchBar = document.createElement("div");
    searchBar.className = "fg-search-bar";
    const isMac = /Mac|iPhone|iPad/.test(navigator.platform);
    searchBar.innerHTML =
      '<input type="text" class="fg-search-input" placeholder="Search frames... (' +
      (isMac ? '\u2318' : 'Ctrl+') + 'F or /)" />' +
      '<span class="fg-search-clear" title="Clear search">\u00d7</span>' +
      '<span class="fg-search-stats"></span>' +
      '<select class="fg-spawn-filter"></select>' +
      '<span class="fg-help-btn" tabindex="0" role="button" title="Keyboard shortcuts">\u2139\ufe0f</span>';
    container.appendChild(searchBar);

    const searchInput = searchBar.querySelector(".fg-search-input");
    const searchClear = searchBar.querySelector(".fg-search-clear");
    const searchStats = searchBar.querySelector(".fg-search-stats");
    const spawnFilter = searchBar.querySelector(".fg-spawn-filter");
    const helpBtn = searchBar.querySelector(".fg-help-btn");

    const helpOverlay = document.createElement("div");
    helpOverlay.className = "fg-help-overlay";
    helpOverlay.innerHTML =
      '<div class="fg-help-content">' +
      '<h3>\u2328 Flamegraph Shortcuts</h3>' +
      '<table>' +
      '<tr><td class="fg-help-key">Click</td><td>Zoom into frame</td></tr>' +
      '<tr><td class="fg-help-key">Right-click</td><td>Zoom out one level</td></tr>' +
      '<tr><td class="fg-help-key">' + (isMac ? '\u2318' : 'Ctrl+') + 'F or /</td><td>Search frames</td></tr>' +
      '<tr><td class="fg-help-key">Esc</td><td>Clear search \u2192 reset zoom \u2192 close</td></tr>' +
      '</table>' +
      '<div class="fg-help-dismiss">Press Esc or click outside to close</div>' +
      '</div>';
    helpOverlay.style.display = "none";
    container.appendChild(helpOverlay);

    helpBtn.addEventListener("click", function () {
      helpOverlay.style.display = helpOverlay.style.display === "none" ? "flex" : "none";
    });
    helpOverlay.addEventListener("click", function (e) {
      if (e.target === helpOverlay) helpOverlay.style.display = "none";
    });

    searchClear.style.display = "none";
    searchClear.addEventListener("click", function () {
      searchInput.value = "";
      searchQuery = "";
      searchClear.style.display = "none";
      repaint();
      searchInput.focus();
    });

    const breadcrumbBar = document.createElement("div");
    breadcrumbBar.className = "fg-breadcrumb";
    container.appendChild(breadcrumbBar);

    container.style.position = container.style.position || "relative";

    const body = document.createElement("div");
    body.className = "fg-body";
    container.appendChild(body);

    const workerLabel = document.createElement("div");
    workerLabel.className = "fg-section-label";
    workerLabel.textContent = "Worker threads";
    body.appendChild(workerLabel);

    const workerCanvas = document.createElement("canvas");
    workerCanvas.className = "fg-canvas";
    body.appendChild(workerCanvas);

    const offworkerLabel = document.createElement("div");
    offworkerLabel.className = "fg-section-label";
    offworkerLabel.textContent = "Off-worker (sampler thread)";
    body.appendChild(offworkerLabel);

    const offworkerCanvas = document.createElement("canvas");
    offworkerCanvas.className = "fg-canvas";
    body.appendChild(offworkerCanvas);

    const tooltip = document.createElement("div");
    tooltip.className = "fg-tooltip";
    document.body.appendChild(tooltip);

    function renderCanvas(canvas, data, hitKey) {
      if (!data) {
        canvas.width = 0;
        canvas.height = 0;
        hitRegions[hitKey] = [];
        return;
      }
      const dpr = devicePixelRatio || 1;
      const pw = canvas.parentElement.clientWidth;
      const ph = (data.maxDepth + 2) * FG_ROW_H + 8;
      canvas.width = pw * dpr;
      canvas.height = ph * dpr;
      canvas.style.width = pw + "px";
      canvas.style.height = ph + "px";
      const ctx = canvas.getContext("2d");
      ctx.scale(dpr, dpr);
      ctx.fillStyle = "#1a1a2e";
      ctx.fillRect(0, 0, pw, ph);

      const regions = [];
      const padL = 4, padR = 4, drawW = pw - padL - padR;
      const baseY = ph - 4;
      ctx.font = "11px monospace";
      ctx.textBaseline = "middle";
      const qLower = searchQuery.toLowerCase();
      const searching = searchQuery.length > 0;

      for (const node of data.nodes) {
        const x = padL + node.x * drawW;
        const w = node.w * drawW;
        const y = baseY - (node.depth + 1) * FG_ROW_H;
        if (w < 0.5) continue;

        const searchMatch = !searching || node.name.toLowerCase().includes(qLower);
        const highlighted = highlightName != null && node.name === highlightName;
        const dimmed = (searching && !searchMatch) || (highlightName != null && !highlighted);
        ctx.globalAlpha = dimmed ? 0.25 : 1.0;
        ctx.fillStyle = flamegraphColor(node.name);
        ctx.fillRect(x, y, Math.max(w - 0.5, 0.5), FG_ROW_H - 1);
        regions.push({ x1: x, x2: x + w, y, node, totalSamples: data.totalSamples });

        if (w > 30) {
          ctx.fillStyle = "#fff";
          const label = node.name.length * 7 > w - 4
            ? node.name.slice(0, Math.floor((w - 10) / 7)) + "\u2026"
            : node.name;
          ctx.save();
          ctx.beginPath();
          ctx.rect(x + 2, y, w - 4, FG_ROW_H);
          ctx.clip();
          ctx.fillText(label, x + 3, y + FG_ROW_H / 2);
          ctx.restore();
        }
      }
      ctx.globalAlpha = 1.0;
      hitRegions[hitKey] = regions;
    }

    function rebuildData(key) {
      const tree = key === "worker" ? workerTree : offworkerTree;
      const stack = key === "worker" ? workerZoomStack : offworkerZoomStack;
      if (!tree) return null;
      const zoomed = stack.length > 0;
      const zoomNode = zoomed ? stack[stack.length - 1] : tree;
      const flat = flattenFromNode(zoomNode, zoomNode.count, zoomed);
      return {
        nodes: flat.nodes,
        maxDepth: flat.maxDepth,
        totalSamples: zoomNode.count,
      };
    }

    function renderAll() {
      workerData = rebuildData("worker");
      offworkerData = rebuildData("offworker");

      workerLabel.style.display = workerData ? "" : "none";
      workerCanvas.style.display = workerData ? "" : "none";
      offworkerLabel.style.display = offworkerData ? "" : "none";
      offworkerCanvas.style.display = offworkerData ? "" : "none";

      repaint();
      renderBreadcrumb();
    }

    function repaint() {
      renderCanvas(workerCanvas, workerData, "worker");
      renderCanvas(offworkerCanvas, offworkerData, "offworker");
      updateSearchStats();
    }

    function renderBreadcrumb() {
      const wZoomed = workerZoomStack.length > 0;
      const oZoomed = offworkerZoomStack.length > 0;
      if (!wZoomed && !oZoomed) {
        breadcrumbBar.style.display = "none";
        return;
      }
      breadcrumbBar.style.display = "flex";
      breadcrumbBar.innerHTML = "";

      if (wZoomed) renderBreadcrumbFor("worker", workerZoomStack);
      if (oZoomed) {
        if (wZoomed) {
          const sep = document.createElement("span");
          sep.textContent = "  |  ";
          sep.style.color = "#555";
          breadcrumbBar.appendChild(sep);
        }
        renderBreadcrumbFor("offworker", offworkerZoomStack);
      }
    }

    function renderBreadcrumbFor(key, stack) {
      const rootSpan = document.createElement("span");
      rootSpan.className = "fg-breadcrumb-item fg-breadcrumb-link";
      rootSpan.textContent = "(all)";
      rootSpan.addEventListener("click", () => {
        if (key === "worker") workerZoomStack = [];
        else offworkerZoomStack = [];
        renderAll();
        onZoomChange();
      });
      breadcrumbBar.appendChild(rootSpan);

      for (let i = 0; i < stack.length; i++) {
        const arrow = document.createElement("span");
        arrow.className = "fg-breadcrumb-sep";
        arrow.textContent = " \u203a ";
        breadcrumbBar.appendChild(arrow);

        const span = document.createElement("span");
        const isLast = i === stack.length - 1;
        span.className = "fg-breadcrumb-item" + (isLast ? "" : " fg-breadcrumb-link");
        span.textContent = stack[i].name;
        span.title = stack[i].name;
        if (!isLast) {
          const idx = i;
          span.addEventListener("click", () => {
            if (key === "worker") workerZoomStack = workerZoomStack.slice(0, idx + 1);
            else offworkerZoomStack = offworkerZoomStack.slice(0, idx + 1);
            renderAll();
            onZoomChange();
          });
        }
        breadcrumbBar.appendChild(span);
      }
    }

    function updateSearchStats() {
      if (!searchQuery) {
        searchStats.textContent = "";
        return;
      }
      const qLower = searchQuery.toLowerCase();
      let matchedSelf = 0;
      let matchedFrames = 0;
      let totalSelf = 0;
      const wRoot = workerZoomStack.length > 0 ? workerZoomStack[workerZoomStack.length - 1] : workerTree;
      const oRoot = offworkerZoomStack.length > 0 ? offworkerZoomStack[offworkerZoomStack.length - 1] : offworkerTree;
      if (wRoot) {
        const m = countSearchMatches(wRoot, qLower);
        matchedSelf += m.selfCount;
        matchedFrames += m.frameCount;
        totalSelf += wRoot.count;
      }
      if (oRoot) {
        const m = countSearchMatches(oRoot, qLower);
        matchedSelf += m.selfCount;
        matchedFrames += m.frameCount;
        totalSelf += oRoot.count;
      }
      if (matchedFrames === 0) {
        searchStats.textContent = "no matches";
        return;
      }
      let text = matchedFrames + (matchedFrames === 1 ? " frame" : " frames");
      if (matchedSelf > 0 && totalSelf > 0) {
        const pct = ((matchedSelf / totalSelf) * 100).toFixed(1);
        text += ` \u00b7 ${pct}% of samples`;
      }
      searchStats.textContent = text;
    }

    searchInput.addEventListener("input", onSearchInput);
    function onSearchInput() {
      searchQuery = searchInput.value;
      searchClear.style.display = searchQuery ? "" : "none";
      repaint();
    }

    function zoomTo(key, treeNode) {
      if (!treeNode || treeNode.children.size === 0) return;
      if (key === "worker") workerZoomStack.push(treeNode);
      else offworkerZoomStack.push(treeNode);
      renderAll();
      onZoomChange();
    }

    function resetZoom() {
      workerZoomStack = [];
      offworkerZoomStack = [];
      renderAll();
      onZoomChange();
    }

    function isZoomed() {
      return workerZoomStack.length > 0 || offworkerZoomStack.length > 0;
    }

    function canvasMouseMove(e, hitKey) {
      const c = e.target;
      const rect = c.getBoundingClientRect();
      const mx = e.clientX - rect.left;
      const my = e.clientY - rect.top;
      const regions = hitRegions[hitKey] || [];
      let hit = null;
      for (let i = regions.length - 1; i >= 0; i--) {
        const r = regions[i];
        if (mx >= r.x1 && mx <= r.x2 && my >= r.y && my < r.y + FG_ROW_H) {
          hit = r;
          break;
        }
      }
      const newHighlight = hit ? hit.node.name : null;
      if (newHighlight !== highlightName) {
        highlightName = newHighlight;
        if (!repaintQueued) {
          repaintQueued = true;
          requestAnimationFrame(() => { repaintQueued = false; repaint(); });
        }
      }
      if (hit) {
        const total = hit.totalSamples;
        const pct = ((hit.node.count / total) * 100).toFixed(1);
        const selfPct = ((hit.node.self / total) * 100).toFixed(1);
        const nameElt = document.createElement("b");
        nameElt.innerText = hit.node.name;
        tooltip.innerHTML = `${nameElt.outerHTML}<br>${hit.node.count} samples (${pct}%) \u00b7 ${hit.node.self} self (${selfPct}%)`;
        tooltip.style.display = "block";
        tooltip.style.left = Math.min(e.clientX + 12, window.innerWidth - 620) + "px";
        tooltip.style.top = (e.clientY - 50) + "px";
        c.style.cursor = "pointer";
      } else {
        tooltip.style.display = "none";
        c.style.cursor = "";
      }
    }

    function canvasMouseLeave() {
      tooltip.style.display = "none";
      if (highlightName !== null) {
        highlightName = null;
        if (!repaintQueued) {
          repaintQueued = true;
          requestAnimationFrame(() => { repaintQueued = false; repaint(); });
        }
      }
    }

    function canvasClick(e, hitKey) {
      const c = e.target;
      const rect = c.getBoundingClientRect();
      const mx = e.clientX - rect.left;
      const my = e.clientY - rect.top;
      const regions = hitRegions[hitKey] || [];
      for (let i = regions.length - 1; i >= 0; i--) {
        const r = regions[i];
        if (mx >= r.x1 && mx <= r.x2 && my >= r.y && my < r.y + FG_ROW_H) {
          if (r.node.treeNode && r.node.treeNode.children.size > 0) {
            zoomTo(hitKey, r.node.treeNode);
          }
          break;
        }
      }
    }

    function canvasContextMenu(e, hitKey) {
      e.preventDefault();
      // Zoom out the canvas you right-clicked, fall back to the other
      const primary = hitKey === "offworker" ? offworkerZoomStack : workerZoomStack;
      const fallback = hitKey === "offworker" ? workerZoomStack : offworkerZoomStack;
      const stack = primary.length > 0 ? primary : fallback;
      if (stack.length > 0) {
        stack.pop();
        renderAll();
        onZoomChange();
      }
    }

    // Named handlers so destroy() can remove them
    function onWorkerMove(e) { canvasMouseMove(e, "worker"); }
    function onOffworkerMove(e) { canvasMouseMove(e, "offworker"); }
    function onWorkerClick(e) { canvasClick(e, "worker"); }
    function onOffworkerClick(e) { canvasClick(e, "offworker"); }
    function onWorkerContext(e) { canvasContextMenu(e, "worker"); }
    function onOffworkerContext(e) { canvasContextMenu(e, "offworker"); }

    workerCanvas.addEventListener("mousemove", onWorkerMove);
    offworkerCanvas.addEventListener("mousemove", onOffworkerMove);
    workerCanvas.addEventListener("mouseleave", canvasMouseLeave);
    offworkerCanvas.addEventListener("mouseleave", canvasMouseLeave);
    workerCanvas.addEventListener("click", onWorkerClick);
    offworkerCanvas.addEventListener("click", onOffworkerClick);
    workerCanvas.addEventListener("contextmenu", onWorkerContext);
    offworkerCanvas.addEventListener("contextmenu", onOffworkerContext);

    function onKeyDown(e) {
      if (container.offsetHeight === 0) return;
      if (((e.ctrlKey || e.metaKey) && e.key === "f") || (e.key === "/" && document.activeElement !== searchInput)) {
        e.preventDefault();
        searchInput.focus();
        searchInput.select();
      }
    }
    document.addEventListener("keydown", onKeyDown);

    // Returns true if consumed (search cleared or zoom reset),
    // false if nothing to do (caller should close the panel).
    function handleEscape() {
      if (helpOverlay.style.display !== "none") {
        helpOverlay.style.display = "none";
        return true;
      }
      if (searchQuery) {
        searchInput.value = "";
        searchQuery = "";
        searchClear.style.display = "none";
        renderAll();
        return true;
      }
      if (isZoomed()) {
        resetZoom(); // resetZoom already calls onZoomChange
        return true;
      }
      return false;
    }

    function applySpawnFilter() {
      const filterVal = spawnFilter.value;
      const samples = filterVal
        ? allSamples.filter((s) => (s.spawnLoc || "(unknown)") === filterVal)
        : allSamples;

      const workerSamples = samples.filter((s) => s.workerId !== 255);
      const offworkerSamples = samples.filter((s) => s.workerId === 255);

      workerTree = workerSamples.length > 0
        ? buildFlamegraphTree(workerSamples, currentSymbols)
        : null;
      offworkerTree = offworkerSamples.length > 0
        ? buildFlamegraphTree(offworkerSamples, currentSymbols)
        : null;

      workerZoomStack = [];
      offworkerZoomStack = [];

      workerLabel.textContent =
        `Worker threads \u2014 ${workerSamples.length} samples`;
      offworkerLabel.textContent =
        `Off-worker (sampler thread) \u2014 ${offworkerSamples.length} samples`;

      renderAll();
    }

    spawnFilter.addEventListener("change", applySpawnFilter);

    function setData(samples, callframeSymbols) {
      allSamples = samples;
      currentSymbols = callframeSymbols;

      // Build spawn location dropdown
      const locCounts = new Map();
      for (const s of samples) {
        const loc = s.spawnLoc || "(unknown)";
        locCounts.set(loc, (locCounts.get(loc) || 0) + 1);
      }
      spawnFilter.innerHTML = '<option value="">All tasks (' + samples.length + ' samples)</option>';
      const sorted = [...locCounts.entries()].sort((a, b) => b[1] - a[1]);
      for (const [loc, count] of sorted) {
        const short = loc.replace(/.*\//, "");
        const opt = document.createElement("option");
        opt.value = loc;
        opt.textContent = short + " (" + count + ")";
        opt.title = loc;
        spawnFilter.appendChild(opt);
      }

      applySpawnFilter();
    }

    function resize() {
      renderCanvas(workerCanvas, workerData, "worker");
      renderCanvas(offworkerCanvas, offworkerData, "offworker");
    }

    function destroy() {
      document.removeEventListener("keydown", onKeyDown);
      workerCanvas.removeEventListener("mousemove", onWorkerMove);
      offworkerCanvas.removeEventListener("mousemove", onOffworkerMove);
      workerCanvas.removeEventListener("mouseleave", canvasMouseLeave);
      offworkerCanvas.removeEventListener("mouseleave", canvasMouseLeave);
      workerCanvas.removeEventListener("click", onWorkerClick);
      offworkerCanvas.removeEventListener("click", onOffworkerClick);
      workerCanvas.removeEventListener("contextmenu", onWorkerContext);
      offworkerCanvas.removeEventListener("contextmenu", onOffworkerContext);
      searchInput.removeEventListener("input", onSearchInput);
      if (tooltip.parentNode) tooltip.parentNode.removeChild(tooltip);
      container.innerHTML = "";
    }

    function getZoomPath() {
      function fullPath(tree, stack) {
        if (!tree || stack.length === 0) return [];
        // If stack already has the full path (from zoomToPath restore), use it directly.
        // Otherwise find the path from root to the zoom target.
        var target = stack[stack.length - 1];
        var path = findNodePath(tree, target.name);
        return path ? path.map((n) => n.name) : stack.map((n) => n.name);
      }
      return {
        worker: fullPath(workerTree, workerZoomStack),
        offworker: fullPath(offworkerTree, offworkerZoomStack),
      };
    }

    // Find a node by name anywhere in the tree via DFS, return path from root.
    function findNodePath(tree, name) {
      const path = [];
      function dfs(node) {
        path.push(node);
        if (node.name === name) return true;
        for (const child of node.children.values()) {
          if (dfs(child)) return true;
        }
        path.pop();
        return false;
      }
      for (const child of tree.children.values()) {
        if (dfs(child)) return path;
      }
      return null;
    }

    function zoomToPath(key, names) {
      const tree = key === "worker" ? workerTree : offworkerTree;
      if (!tree || !names.length) return;
      const stack = key === "worker" ? workerZoomStack : offworkerZoomStack;
      // Try walking child-by-child (works when names is a full path from root)
      let node = tree;
      for (let i = 0; i < names.length; i++) {
        let found = null;
        for (const child of node.children.values()) {
          if (child.name === names[i]) { found = child; break; }
        }
        if (!found) {
          // Path walk failed, fall back to DFS for the last name
          const path = findNodePath(tree, names[names.length - 1]);
          if (path) stack.push.apply(stack, path);
          break;
        }
        stack.push(found);
        node = found;
      }
      if (stack.length > 0) renderAll();
    }

    return { setData, resize, destroy, handleEscape, isZoomed, getZoomPath, zoomToPath };
  }

  const fgExports = { createFlamegraph: createFlamegraph, filterCpuSamples: filterCpuSamples };
  if (typeof module !== "undefined" && module.exports) {
    module.exports = fgExports;
  } else {
    exports.FlamegraphRenderer = fgExports;
  }
})(typeof exports === "undefined" ? this : exports);
