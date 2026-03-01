# dial9-tokio-telemetry

**Low-overhead runtime telemetry for Tokio.** Records poll timing, worker park/unpark, wake events, queue depths, and (on Linux) CPU profile samples into a compact binary trace format. Traces can be analyzed offline to find long polls, scheduling delays, idle workers, and CPU hotspots.

```rust
use dial9_tokio_telemetry::telemetry::{RotatingWriter, TracedRuntime};

fn main() -> std::io::Result<()> {
    let writer = RotatingWriter::new(
        "/tmp/my_traces/trace.bin",
        1024 * 1024,      // rotate after 1 MiB per file
        5 * 1024 * 1024,  // keep at most 5 MiB on disk
    )?;

    let mut builder = tokio::runtime::Builder::new_multi_thread();
    builder.worker_threads(4).enable_all();

    let (runtime, _guard) = TracedRuntime::build_and_start(builder, Box::new(writer))?;

    runtime.block_on(async {
        // your async code here
    });

    Ok(())
}
```

Events are 6–16 bytes on the wire, and a typical request generates ~20–35 bytes of trace data (a few poll events plus park/unpark). At 10k requests/sec that's well under 1 MB/s — `RotatingWriter` caps total disk usage so you can leave it running indefinitely. Typical CPU overhead is under 5%.

> **Note:** dial9-tokio-telemetry is designed for always-on production use, but it's still early software. Measure overhead and validate behavior in your environment before deploying to production.

## Is there a demo?
Yes, checkout this [quick walkthrough (YouTube)](https://www.youtube.com/watch?v=zJOzU_6Mf7Q)!

## Why dial9-tokio-telemetry?

Understanding how Tokio is actually running your application — which tasks are slow, why workers are idle, where scheduling delays come from — is hard to do from the outside. This crate records a continuous, low-overhead trace of runtime behavior.

Compared to [tokio-console](https://github.com/tokio-rs/console), which is designed for live debugging, dial9-tokio-telemetry is designed for post-hoc analysis. Because traces are written to files with bounded disk usage, you can leave it running in production and come back later to deeply analyze what went wrong or why a specific request was slow. On Linux, traces include CPU profile samples and scheduler events, so you can see not just *that* a task was delayed but *what code* was running on the worker instead.

## What gets recorded automatically

`TracedRuntime` installs hooks on the Tokio runtime builder. These fire for every task on the runtime with no code changes required:

| Event | Fields |
|-------|--------|
| `PollStart` / `PollEnd` | timestamp, worker, task ID, spawn location, local queue depth |
| `WorkerPark` / `WorkerUnpark` | timestamp, worker, local queue depth, thread CPU time, schedstat wait |
| `QueueSample` | timestamp, global queue depth (sampled every 10 ms) |
| `TaskSpawn` / `SpawnLocationDef` | task→spawn-location mapping (when `task_tracking` is enabled) |

## Wake event tracking

Wake events — which task woke which other task — are *not* captured automatically. Tokio's runtime hooks don't expose waker identity, so capturing this requires wrapping the future in `Traced<F>`, which installs a custom waker that records a `WakeEvent` before forwarding to the real waker.

Use `handle.spawn()` instead of `tokio::spawn()`:

```rust,no_run
# use dial9_tokio_telemetry::telemetry::{RotatingWriter, TracedRuntime};
# fn main() -> std::io::Result<()> {
# let writer = RotatingWriter::new("/tmp/t.bin", 1024, 4096)?;
# let builder = tokio::runtime::Builder::new_multi_thread();
let (runtime, guard) = TracedRuntime::build_and_start(builder, Box::new(writer))?;
let handle = guard.handle();

runtime.block_on(async {
    // wake events captured — uses Traced<F> wrapper
    handle.spawn(async { /* ... */ });

    // wake events NOT captured — still gets poll/park/queue telemetry
    tokio::spawn(async { /* ... */ });
});
# Ok(())
# }
```

For frameworks like Axum where you don't control the spawn call, you need to wrap the accept loop. See [`examples/metrics-service/src/axum_traced.rs`](/examples/metrics-service/src/axum_traced.rs) for a working example that wraps both the accept loop and per-connection futures.

## Platform support

Core telemetry (poll timing, park/unpark, queue depth, wake events) works on all platforms.

On Linux, you get additional data for free:
- **Thread CPU time** in park/unpark events via `CLOCK_THREAD_CPUTIME_ID` (vDSO, ~20–40 ns)
- **Scheduler wait time** via `/proc/self/task/<tid>/schedstat` — shows how long the OS kept your thread off-CPU

On non-Linux platforms these fields are zero.

### CPU profiling (Linux only)

With the `cpu-profiling` feature, you can enable `perf_event_open`-based CPU sampling and scheduler event capture. This records stack traces attributed to specific worker threads, so you can see *what code* was running during a scheduling delay.

```rust,no_run
# #[cfg(feature = "cpu-profiling")]
# fn main() -> std::io::Result<()> {
# use dial9_tokio_telemetry::telemetry::{RotatingWriter, TracedRuntime};
use dial9_tokio_telemetry::telemetry::{CpuProfilingConfig, SchedEventConfig};

# let writer = RotatingWriter::new("/tmp/t.bin", 1024, 4096)?;
# let builder = tokio::runtime::Builder::new_multi_thread();
let (runtime, guard) = TracedRuntime::builder()
    .with_task_tracking(true)
    .with_cpu_profiling(CpuProfilingConfig::default())
    .with_sched_events(SchedEventConfig { include_kernel: true })
    .with_inline_callframe_symbols(true)
    .build(builder, Box::new(writer))?;
# Ok(())
# }
# #[cfg(not(feature = "cpu-profiling"))]
# fn main() {}
```

This pulls in [`dial9-perf-self-profile`](/perf-self-profile) for `perf_event_open` access. It records `CpuSample` events with full callchains and `CallframeDef` / `ThreadNameDef` metadata for offline symbolization.

## Getting started

`TracedRuntime::build` returns a `(Runtime, TelemetryGuard)`. The guard owns the flush thread and provides a `TelemetryHandle` for enabling/disabling recording at runtime:

```rust,no_run
# use dial9_tokio_telemetry::telemetry::{RotatingWriter, TracedRuntime};
# fn main() -> std::io::Result<()> {
# let writer = RotatingWriter::new("/tmp/t.bin", 1024, 4096)?;
# let builder = tokio::runtime::Builder::new_multi_thread();
let (runtime, guard) = TracedRuntime::builder()
    .with_task_tracking(true)
    .build(builder, Box::new(writer))?;

// start disabled, enable later
guard.enable();

// TelemetryHandle is Clone + Send — pass it around
let handle = guard.handle();
handle.disable();
# Ok(())
# }
```

### Writers

`RotatingWriter` is what you want for production — it rotates files and evicts old ones to stay within a total size budget. `SimpleBinaryWriter` writes a single file with no size management, useful for quick experiments. `NullWriter` measures hook overhead without doing any I/O.

### Analyzing traces

```bash
# per-worker stats, wake→poll delays, idle worker detection
cargo run --example analyze_trace -- /tmp/my_traces/trace.0.bin

# convert to JSONL for ad-hoc scripting
cargo run --example trace_to_jsonl -- /tmp/my_traces/trace.0.bin output.jsonl
```

There's also an interactive HTML trace viewer — open `trace_viewer/index.html` and drag in a `.bin` file. [Here's a demo.](https://www.youtube.com/watch?v=zJOzU_6Mf7Q)

See [TRACE_ANALYSIS_GUIDE.md](/dial9-tokio-telemetry/TRACE_ANALYSIS_GUIDE.md) for a walkthrough of diagnosing scheduling delays and CPU hotspots from trace data.

## Features

- **`cpu-profiling`** — Linux only. Enables `perf_event_open`-based CPU sampling and scheduler event capture via `dial9-perf-self-profile`.
- **`task-dump`** — Enables Tokio's `taskdump` feature for async stack traces. Required for the `long_sleep`, `completing_task`, `cancelled_task`, and `debug_timing` examples.

## Examples

```bash
cargo run --example telemetry_rotating   # rotating writer demo
cargo run --example simple_workload      # basic instrumented workload
cargo run --example realistic_workload   # mixed CPU/IO workload
cargo run --example long_workload        # longer run for trace analysis
```

The [`examples/metrics-service`](/examples/metrics-service) directory has a full Axum service with DynamoDB persistence, a load-generating client, and telemetry wired up end-to-end.

## Overhead

```bash
./scripts/compare_overhead.sh [duration_secs]
```

This runs the `overhead_bench` binary with and without telemetry and reports the difference. Typical output:

```text
Baseline:   286794 req/s, p50=174.1µs, p99=280.6µs
Telemetry:  277626 req/s, p50=180.2µs, p99=289.3µs
Overhead:   3.2%
```

## Workspace

This repo is a Cargo workspace with three members:

- [`dial9-tokio-telemetry`](/dial9-tokio-telemetry) — the main crate
- [`dial9-perf-self-profile`](/perf-self-profile) — minimal Linux `perf_event_open` wrapper for CPU profiling and scheduler events
- [`examples/metrics-service`](/examples/metrics-service) — end-to-end example service

## Future work

- **S3 writer** — upload traces directly to S3 instead of relying on log shipping
- **Parquet output** — write traces as Parquet for efficient querying with Athena, DuckDB, etc.
- **Tokio task dumps** — capture async stack traces of all in-flight tasks
- **Retroactive sampling** — trace data lives in a ring buffer; when your application detects anomalous behavior, it triggers persistence of the last N seconds of data rather than recording everything continuously
- **Out-of-process symbolication** — resolve CPU profile stack traces in a background process to avoid adding latency or memory overhead to the application

## License

This project is licensed under the Apache-2.0 License.
