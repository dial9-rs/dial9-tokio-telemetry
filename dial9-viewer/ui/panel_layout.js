// panel_layout.js — shared time-panel layout math.
//
// Every time-based panel in the viewer (timeline, worker lanes, span panel,
// task detail, queue chart) must agree on the horizontal layout so their time
// axes line up vertically:
//
//   ┌──────────────┬──────────────────────────────┬────────┐
//   │  label area  │       draw area              │ scroll │
//   │   LABEL_W    │   drawW = W - LABEL_W - sb   │   sb   │
//   └──────────────┴──────────────────────────────┴────────┘
//   x=0          x=LABEL_W                     x=W-sb    x=W
//
// The bug this module prevents: a panel redefines the left-gutter width
// (e.g. `padding-left: 200px`) or forgets to add LABEL_W when drawing.  Either
// mistake shifts the panel's time axis relative to every other panel — bars
// no longer line up with their corresponding polls, confusing users who are
// trying to correlate events across panels.
//
// Keep this file pure (no DOM access) so it's trivially unit-testable.  The
// browser-side `timePanelLayout(panel, scrollbarW)` in viewer.html wraps this
// with DOM-reading and canvas-sizing on top.

(function (exports) {
  "use strict";

  /**
   * Build a time-panel layout view.  Pure function — no globals, no DOM.
   *
   * @param {number} pw          Full panel/canvas width in CSS px.
   * @param {number} labelW      Left gutter width reserved for labels
   *                             (by convention, LABEL_W = 100).
   * @param {number} scrollbarW  Right gutter for the lane scrollbar.  Pass 0
   *                             for panels that don't need to match the lane
   *                             right edge.
   * @param {number} viewStart   Visible-range start timestamp (ns).
   * @param {number} viewEnd     Visible-range end timestamp (ns).
   * @returns {{
   *   pw: number, labelW: number, drawW: number,
   *   nsToPanelX: (ns: number) => number,
   *   nsToPanelXClamped: (ns: number) => number,
   *   panelXToNs: (x: number) => number,
   * }}
   */
  function makeTimePanelLayout(pw, labelW, scrollbarW, viewStart, viewEnd) {
    const sb = scrollbarW || 0;
    const drawW = pw - labelW - sb;
    const span = (viewEnd - viewStart) || 1;
    return {
      pw,
      labelW,
      drawW,
      /** Convert a timestamp to a canvas x-coordinate (labelW-shifted). */
      nsToPanelX(ns) {
        return labelW + ((ns - viewStart) / span) * drawW;
      },
      /** Like nsToPanelX, but clamped to [labelW, labelW+drawW]. */
      nsToPanelXClamped(ns) {
        const raw = ((ns - viewStart) / span) * drawW;
        return labelW + Math.max(0, Math.min(drawW, raw));
      },
      /** Convert a canvas x-coordinate back to a timestamp. */
      panelXToNs(x) {
        return viewStart + ((x - labelW) / drawW) * span;
      },
    };
  }

  exports.makeTimePanelLayout = makeTimePanelLayout;
})(
  typeof module !== "undefined" && module.exports
    ? module.exports
    : (typeof window !== "undefined"
        ? (window.PanelLayout = window.PanelLayout || {})
        : (this.PanelLayout = this.PanelLayout || {})),
);
