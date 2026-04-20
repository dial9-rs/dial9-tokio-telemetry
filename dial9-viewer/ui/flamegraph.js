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
  function flattenFromNode(root, total) {
    const nodes = [];
    let maxD = 0;
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
      walk(child, 0, cx);
      cx += child.count / total;
    }
    return { nodes, maxDepth: maxD };
  }

  // Sum self-sample counts for frames whose name matches the query.
  // Uses self (not inclusive count) to avoid double-counting in the percentage.
  function countSearchMatches(root, queryLower) {
    let matched = 0;
    function walk(node) {
      if (node.name.toLowerCase().includes(queryLower)) {
        matched += node.self;
      }
      for (const child of node.children.values()) walk(child);
    }
    walk(root);
    return matched;
  }

  function createFlamegraph(container) {
    let workerTree = null;
    let offworkerTree = null;
    let workerData = null;
    let offworkerData = null;
    let workerZoomStack = [];
    let offworkerZoomStack = [];
    let searchQuery = "";
    const hitRegions = { worker: [], offworker: [] };

    // DOM
    const searchBar = document.createElement("div");
    searchBar.className = "fg-search-bar";
    const isMac = /Mac|iPhone|iPad/.test(navigator.platform);
    searchBar.innerHTML =
      '<input type="text" class="fg-search-input" placeholder="Search frames... (' +
      (isMac ? '\u2318' : 'Ctrl+') + 'F or /)" />' +
      '<span class="fg-search-stats"></span>';
    container.appendChild(searchBar);

    const searchInput = searchBar.querySelector(".fg-search-input");
    const searchStats = searchBar.querySelector(".fg-search-stats");

    const breadcrumbBar = document.createElement("div");
    breadcrumbBar.className = "fg-breadcrumb";
    container.appendChild(breadcrumbBar);

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

        const matches = !searching || node.name.toLowerCase().includes(qLower);
        ctx.globalAlpha = searching && !matches ? 0.25 : 1.0;
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
      const zoomNode = stack.length > 0 ? stack[stack.length - 1] : tree;
      const flat = flattenFromNode(zoomNode, zoomNode.count);
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
      let totalSelf = 0;
      if (workerTree) {
        matchedSelf += countSearchMatches(workerTree, qLower);
        totalSelf += workerTree.count;
      }
      if (offworkerTree) {
        matchedSelf += countSearchMatches(offworkerTree, qLower);
        totalSelf += offworkerTree.count;
      }
      if (totalSelf === 0) {
        searchStats.textContent = "";
        return;
      }
      const pct = ((matchedSelf / totalSelf) * 100).toFixed(1);
      searchStats.textContent = `${pct}% self time (${matchedSelf} samples)`;
    }

    searchInput.addEventListener("input", onSearchInput);
    function onSearchInput() {
      searchQuery = searchInput.value;
      repaint();
    }

    function zoomTo(key, treeNode) {
      if (!treeNode || treeNode.children.size === 0) return;
      if (key === "worker") workerZoomStack.push(treeNode);
      else offworkerZoomStack.push(treeNode);
      renderAll();
    }

    function resetZoom() {
      workerZoomStack = [];
      offworkerZoomStack = [];
      renderAll();
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

    // Named handlers so destroy() can remove them
    function onWorkerMove(e) { canvasMouseMove(e, "worker"); }
    function onOffworkerMove(e) { canvasMouseMove(e, "offworker"); }
    function onWorkerClick(e) { canvasClick(e, "worker"); }
    function onOffworkerClick(e) { canvasClick(e, "offworker"); }

    workerCanvas.addEventListener("mousemove", onWorkerMove);
    offworkerCanvas.addEventListener("mousemove", onOffworkerMove);
    workerCanvas.addEventListener("mouseleave", canvasMouseLeave);
    offworkerCanvas.addEventListener("mouseleave", canvasMouseLeave);
    workerCanvas.addEventListener("click", onWorkerClick);
    offworkerCanvas.addEventListener("click", onOffworkerClick);

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
      if (searchQuery) {
        searchInput.value = "";
        searchQuery = "";
        renderAll();
        return true;
      }
      if (isZoomed()) {
        resetZoom();
        return true;
      }
      return false;
    }

    function setData(workerSamples, offworkerSamples, callframeSymbols) {
      workerTree = workerSamples.length > 0
        ? buildFlamegraphTree(workerSamples, callframeSymbols)
        : null;
      offworkerTree = offworkerSamples.length > 0
        ? buildFlamegraphTree(offworkerSamples, callframeSymbols)
        : null;

      workerZoomStack = [];
      offworkerZoomStack = [];

      workerLabel.textContent =
        `Worker threads \u2014 ${workerSamples.length} samples`;
      offworkerLabel.textContent =
        `Off-worker (sampler thread) \u2014 ${offworkerSamples.length} samples`;

      renderAll();
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
      searchInput.removeEventListener("input", onSearchInput);
      if (tooltip.parentNode) tooltip.parentNode.removeChild(tooltip);
      container.innerHTML = "";
    }

    return { setData, resize, destroy, handleEscape, isZoomed };
  }

  const fgExports = { createFlamegraph: createFlamegraph };
  if (typeof module !== "undefined" && module.exports) {
    module.exports = fgExports;
  } else {
    exports.FlamegraphRenderer = fgExports;
  }
})(typeof exports === "undefined" ? this : exports);
