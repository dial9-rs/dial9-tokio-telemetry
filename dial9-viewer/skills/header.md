# dial9 Trace Analysis Skill

dial9 traces capture the internal behavior of a Tokio async runtime: task polling, worker thread activity, queue depths, CPU profiling samples, scheduling delays, and task lifecycle events. You can analyze them programmatically using Node.js.

## What traces capture

- **Poll events**: Every time a worker thread polls a task future (start/end timestamps, task ID, spawn location)
- **Worker lifecycle**: Park/unpark events with CPU time and kernel scheduling wait
- **Queue depth**: Periodic samples of the global injection queue
- **Task lifecycle**: Spawn and terminate events with spawn location
- **Wake events**: Which task woke which other task, and on which worker
- **CPU samples**: Periodic stack traces from perf/eBPF, attached to the poll they occurred in
- **Scheduling samples**: Stack traces captured when the kernel deschedules a worker thread (shows blocking calls)
- **Clock sync**: Monotonic-to-wall-clock anchors for correlating with external logs
- **Span events**: Enter/exit events from `tracing` spans (`#[instrument]`), showing what happened inside each poll with field values and nesting

## Instrumenting your app

### 1. Add the dependency

```toml
[dependencies]
dial9-tokio-telemetry = { version = "0.3", features = ["tracing-layer"] }
tracing = "0.1"
tracing-subscriber = { version = "0.3", features = ["env-filter"] }
```

### 2. Set up the traced runtime and tracing layer

Replace your `tokio::runtime::Builder` with `TracedRuntime` and install `Dial9TokioLayer`:

```rust,ignore
use dial9_tokio_telemetry::telemetry::{RotatingWriter, TracedRuntime};
use dial9_tokio_telemetry::tracing_layer::Dial9TokioLayer;
use tracing_subscriber::prelude::*;

// Set up the tracing subscriber with the dial9 layer
tracing_subscriber::registry()
    .with(tracing_subscriber::fmt::layer())
    .with(
        Dial9TokioLayer::new().with_filter(
            tracing_subscriber::filter::Targets::new()
                .with_target("my_app", tracing::Level::TRACE)
                .with_default(tracing::Level::ERROR),
        ),
    )
    .init();

// Wrap your runtime with TracedRuntime
let writer = RotatingWriter::single_file("trace.bin")?;
let mut builder = tokio::runtime::Builder::new_multi_thread();
builder.worker_threads(4).enable_all();
let (runtime, guard) = TracedRuntime::build_and_start(builder, writer)?;

runtime.block_on(async { /* your app */ });

drop(runtime);
drop(guard); // flushes the trace
// Trace is now at trace.0.bin
```

### 3. Add `#[instrument]` to functions you want to trace

```rust,ignore
#[tracing::instrument(skip_all, fields(request_id = %id))]
async fn handle_request(id: String, db: &Db) {
    // spans are recorded automatically on enter/exit
}
```

Filter to your app's target to avoid noise from third-party crates (AWS SDK, hyper, etc.).

## Quick start (analysis)

Get the analysis toolkit:

```bash
dial9-viewer agents toolkit /tmp/d9-toolkit
node /tmp/d9-toolkit/analyze.js <trace.bin>
```

This copies `decode.js`, `trace_parser.js`, `trace_analysis.js`, and `analyze.js` into the target directory. Run `analyze.js` for a full diagnostic report, then edit any of the files to drill deeper.

### Parsing traces manually

```javascript
const fs = require('fs');
const { parseTrace, EVENT_TYPES } = require('./trace_parser.js');
const { buildWorkerSpans, attachCpuSamples, buildActiveTaskTimeline,
        computeSchedulingDelays, filterPointsOfInterest, buildFgData } = require('./trace_analysis.js');

const buf = fs.readFileSync('trace.bin');
const trace = await parseTrace(buf);

// Get worker IDs
const workerIds = [...new Set(
  trace.events.filter(e => e.eventType !== EVENT_TYPES.QueueSample && e.eventType !== EVENT_TYPES.WakeEvent)
    .map(e => e.workerId)
)].sort((a, b) => a - b);

const minTs = trace.events.reduce((m, e) => Math.min(m, e.timestamp), Infinity);
const maxTs = trace.events.reduce((m, e) => Math.max(m, e.timestamp), -Infinity);

// Build the full analysis pipeline
const spans = buildWorkerSpans(trace.events, workerIds, maxTs);
attachCpuSamples(trace.cpuSamples, spans.workerSpans);
const taskTimeline = buildActiveTaskTimeline(trace.taskSpawnTimes, trace.taskTerminateTimes);
const schedDelays = computeSchedulingDelays(spans.workerSpans, workerIds, spans.wakesByTask);
```

## Fetching traces from S3

If `dial9-viewer` is running (e.g. on port 3000), fetch traces via its API:

```javascript
// Search for traces
const resp = await fetch('http://localhost:3000/api/search?bucket=BUCKET&q=2026-04-09/19');
const objects = await resp.json(); // [{key, size, last_modified}, ...]

// Fetch and parse a trace (server gunzips and concatenates segments)
const keys = objects.map(o => `keys=${encodeURIComponent(o.key)}`).join('&');
const traceResp = await fetch(`http://localhost:3000/api/trace?bucket=BUCKET&${keys}`);
const buf = Buffer.from(await traceResp.arrayBuffer());
const trace = await parseTrace(buf);
```

## Available skill segments

Run `dial9-viewer agents <segment>` for detailed information:

| Command / Segment | Description |
|-------------------|-------------|
| `agents toolkit DIR` | **Start here.** Copies the analysis toolkit to a directory |
| `agents skill setup` | How to add dial9 and the tracing layer to your app |
| `agents skill runtime` | Tokio runtime internals: execution model, scheduling, wake/poll lifecycle, and how to fix common problems |
| `agents skill loading` | Trace format details, parsing options, time range filtering |
| `agents skill analysis` | Full analysis pipeline API reference |
| `agents skill recipes` | Diagnostic recipes for common questions |
| `agents skill red-flags` | Automated checks for common runtime problems |
