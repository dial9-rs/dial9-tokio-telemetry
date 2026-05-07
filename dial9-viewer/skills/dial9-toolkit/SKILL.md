---
name: dial9-toolkit
description: JavaScript analysis toolkit for parsing and analyzing dial9 Tokio runtime traces. Provides trace_parser.js, trace_analysis.js, and analyze.js. Use when you need to load, parse, or programmatically analyze dial9 trace files.
---

# dial9 Analysis Toolkit

This skill provides the JavaScript modules for working with dial9 traces programmatically.

## Quick start

```bash
node scripts/analyze.js <trace.bin or directory>  # full diagnostic report
node scripts/analyze.js traces/ --sample 50       # quick overview of large directories
node scripts/analyze.js trace.bin --force          # ignore cached results
```

## Modules

| File | Purpose |
|------|---------|
| `scripts/analyze.js` | CLI entry point and `analyzeTraces()` aggregation function |
| `scripts/trace_parser.js` | Binary parser: `parseTrace(path)` yields `ParsedTrace` objects |
| `scripts/trace_analysis.js` | Analysis functions: `buildWorkerSpans`, `attachCpuSamples`, etc. |
| `scripts/decode.js` | Low-level binary format decoder |

## Usage from other skills

```javascript
const { analyzeTraces } = require('./scripts/analyze.js');
const result = await analyzeTraces('/path/to/traces/');
// result.longPolls, result.workerSpans, result.schedDelayHist, result.cpuGroups, result.spanStats
```

`analyzeTraces` works on a single file or a directory. It returns a single aggregated result object with everything you need for diagnosis. See the `dial9-trace-analysis` skill for the full return schema.

For drill-down into raw events (specific tasks, custom filtering, wake chains):

```javascript
const { parseTrace } = require('./scripts/trace_parser.js');
const trace = await parseTrace('/path/to/trace.bin');  // single file
// or iterate a directory:
for await (const trace of parseTrace('/path/to/traces/')) { ... }
```
