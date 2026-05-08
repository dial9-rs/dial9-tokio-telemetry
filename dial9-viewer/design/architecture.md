# dial9-viewer design

## Overview

`dial9-viewer` is a trace viewer and analysis toolkit for dial9 runtime traces. It has two modes:

- **Web UI**: Browser-based trace viewer with a Rust/Axum backend for S3 trace storage.
- **Agent toolkit**: JS scripts for programmatic trace analysis. Distributed via `dial9-viewer agents toolkit <dir>`.

## Web UI

The viewer is a single-page app (`viewer.html`) served by a Rust/Axum HTTP server. The server's role is trace storage and retrieval; all parsing and analysis runs client-side in the browser using the same JS libraries (`trace_parser.js`, `trace_analysis.js`) as the agent toolkit.

The server exposes a REST API under `/api/` for browsing S3 prefixes, searching traces by metadata, and fetching trace binaries. The API is usable independently via `curl` or scripts for automation without the browser.

Traces can also be loaded by drag-and-drop or file picker without a server (static file mode).

### Time-panel layout invariant

Every time-based panel in the viewer — timeline header, worker lanes, span panel, task detail, and queue chart — shares the same horizontal layout so its time axis lines up vertically with every other panel:

```
┌──────────────┬──────────────────────────────┬────────┐
│  label area  │       draw area              │ scroll │
│   LABEL_W    │   drawW = W - LABEL_W - sb   │   sb   │
└──────────────┴──────────────────────────────┴────────┘
x=0          x=LABEL_W                     x=W-sb    x=W
```

`LABEL_W = 100` is the canonical left-gutter width. The invariant is enforced by the shared helper in `ui/panel_layout.js` (`makeTimePanelLayout`), which produces the coordinate-conversion functions (`nsToPanelX`, `panelXToNs`) used by every panel. The browser-side wrapper `timePanelLayout(panel, scrollbarW)` in `viewer.html` adds DOM-reading and canvas-sizing on top.

Worker lanes are a slight exception: they use a DOM flex layout (`lane-label` div of width `LABEL_W`, then a `lane-content` div hosting the canvas) rather than a single canvas with an internal offset. The end result — time axis starts at x=LABEL_W — is identical; new panels should prefer the `timePanelLayout` pattern.

Regression history: the span panel was once built with `padding-left: 200px` instead of `100px`, shifting its time axis ~100px right of every other panel. `ui/test_panel_layout.js` now guards the invariant with unit tests.

## Agent skills (steering)

The viewer bundles markdown "skills" that teach AI agents how to use the toolkit. These are compiled into the binary at build time by `build.rs`:

- `skills/header.md` → `HEADER` constant (printed by `dial9-viewer agents`)
- `skills/*.md` (except header) → `SKILL_FILES` array (printed by `dial9-viewer agents skill <name>`)
- `toolkit/*` (symlinks to `skills/` and `ui/`) → `TOOLKIT_FILES` array (written by `dial9-viewer agents toolkit <dir>`)

The header is the entry point. An agent runs `dial9-viewer agents`, reads the header, discovers available skill segments and the toolkit command, then copies the toolkit and starts analyzing.

## Agent toolkit architecture

### Single file

`parseTrace(buffer)` decodes the binary trace format (via `decode.js`, a WASM-based decoder) and returns a `ParsedTrace` with events, CPU samples, symbol tables, and task lifecycle data. Analysis functions in `trace_analysis.js` derive higher-level structures (worker spans, scheduling delays, task timelines) from the parsed trace.

Browser mode is single-file only; directory parsing, caching, and subprocess parallelism are Node.js-only.

### Directory (multi-file)

`parseTrace(directoryPath)` returns an async iterable of `ParsedTrace`, one per file:

1. Spawns one subprocess per file (concurrency capped at CPU count)
2. Each subprocess parses the trace and writes the full `ParsedTrace` as NDJSON to `.d9-cache/`
3. The iterator yields one `ParsedTrace` at a time, keeping memory bounded

`for await (const trace of parseTrace(input))` works for both single files and directories. Warm runs read cached NDJSON directly (no subprocesses needed for cached files).

### analyzeTraces (aggregated analysis)

`analyzeTraces(path)` returns aggregated results across all files. Use it for summary statistics; use `parseTrace` when you need per-trace raw data (flamegraphs, field filtering, wake chains). Two parallel phases:

1. **Parse phase**: subprocesses populate the NDJSON cache
2. **Analysis phase**: subprocesses each read one cached file, run the full analysis pipeline, and output a partial accumulator as JSON to stdout

The main process merges partial accumulators via constant-memory streaming merge: summing counts, keeping top-N findings, and feeding values into native histograms for exact percentiles.

See `agents skill analysis` for the full return schema.

If any file fails to parse or analyze, the entire operation fails immediately rather than skipping the file. This is a known limitation; partial failure tolerance may be added in the future.

### Cache format

`.d9-cache/<filename>.json` contains NDJSON (one JSON record per line, tagged by type) to avoid V8's string size limit. The reader splits a Buffer on newlines and parses each line independently.

Cache invalidation is mtime-based. `--force` bypasses the cache.
