# dial9-viewer design

## Overview

`dial9-viewer` is a trace viewer and analysis toolkit for dial9 runtime traces. It has two modes:

- **Web UI**: Rust HTTP server that fetches traces from S3, serves the viewer HTML/JS, and provides search/browse APIs.
- **Agent toolkit**: JS scripts (`analyze.js`, `trace_parser.js`, `trace_analysis.js`) that parse and analyze traces programmatically. Distributed via `dial9-viewer agents toolkit <dir>`.

## Agent skills (steering)

The viewer bundles markdown "skills" that teach AI agents how to use the toolkit. These are compiled into the binary at build time by `build.rs`:

- `skills/header.md` → `HEADER` constant (printed by `dial9-viewer agents`)
- `skills/*.md` (except header) → `SKILL_FILES` array (printed by `dial9-viewer agents skill <name>`)
- `toolkit/*` (symlinks to `skills/` and `ui/`) → `TOOLKIT_FILES` array (written by `dial9-viewer agents toolkit <dir>`)

The header is the entry point. An agent runs `dial9-viewer agents`, reads the header, discovers available skill segments and the toolkit command, then copies the toolkit and starts analyzing.

Non-`.md` files in `skills/` (like `analyze.js`) are not served as skills but are included in the toolkit via symlinks in `toolkit/`.

## Agent toolkit architecture

### Single file

`parseTrace(buffer)` decodes the binary trace format (via `decode.js`, a WASM-based decoder) and returns a `ParsedTrace` with events, CPU samples, symbol tables, and task lifecycle data. Analysis functions in `trace_analysis.js` derive higher-level structures (worker spans, scheduling delays, task timelines) from the parsed trace.

### Directory (multi-file)

`parseTrace(directoryPath)` returns an async iterable of `ParsedTrace`, one per file:

1. Spawns one subprocess per file (concurrency capped at CPU count)
2. Each subprocess parses the trace and writes the full `ParsedTrace` as NDJSON to `.d9-cache/`
3. The iterator yields one `ParsedTrace` at a time, keeping memory bounded

`for await (const trace of parseTrace(input))` works for both single files and directories. For buffers (browser), `parseTrace` returns `Promise<ParsedTrace>`.

Warm runs read cached NDJSON directly (no subprocesses needed for cached files).

### analyzeTraces (aggregated analysis)

`analyzeTraces(path)` returns aggregated results across all files. Two parallel phases:

1. **Parse phase**: subprocesses populate the NDJSON cache
2. **Analysis phase**: subprocesses each read one cached file, run the full analysis pipeline, and output a partial accumulator as JSON to stdout

The main process merges partial accumulators in constant memory: summing counts, keeping top-N long polls, feeding delay/duration values into Node's native `createHistogram()` for exact percentiles.

The result includes: worker utilization, top long polls with stack traces, scheduling delay histograms, poll duration histograms by spawn location, span duration histograms, task lifecycle, CPU/scheduling sample groups, and queue depth stats.

### Cache format

`.d9-cache/<filename>.json` contains NDJSON (one JSON record per line):

| Tag | Content |
|-----|---------|
| `m` | Metadata: scalar fields, Maps as `[k,v]` arrays |
| `e` | Event (one per line) |
| `c` | CPU sample (one per line) |
| `x` | Custom event (one per line) |

NDJSON avoids V8's string size limit. The reader splits a Buffer on newlines and parses each line independently.

Cache invalidation is mtime-based. `--force` bypasses the cache.
