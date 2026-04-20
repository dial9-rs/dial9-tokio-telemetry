# dial9-tokio-telemetry

[![Crates.io](https://img.shields.io/crates/v/dial9-tokio-telemetry.svg)](https://crates.io/crates/dial9-tokio-telemetry)
[![Documentation](https://docs.rs/dial9-tokio-telemetry/badge.svg)](https://docs.rs/dial9-tokio-telemetry)
![License](https://img.shields.io/crates/l/dial9-tokio-telemetry.svg)

**Low-overhead runtime telemetry for Tokio.** Records poll timing, worker park/unpark, wake events, queue depths, and (on Linux) CPU profile samples into a compact binary trace format. Traces can be analyzed offline to find long polls, scheduling delays, idle workers, and CPU hotspots.

## Prerequisites

This crate requires Tokio's unstable APIs for runtime hooks and worker metrics. Add the following to your project's `.cargo/config.toml`:

```toml
# .cargo/config.toml
[build]
rustflags = [
  "--cfg", "tokio_unstable",
  # For profiling, you also need:
  # "-C", "force-frame-pointers=yes"
]
```

Without this flag, compilation will fail with errors about missing methods on `tokio::runtime::Builder` and `RuntimeMetrics`.

## Quick start

There are two ways to set up dial9: the `#[main]` macro (recommended for most apps) or manual `TracedRuntime` setup. The macro handles the boilerplate of building the runtime and spawning your code as an instrumented task. Inside your `main` body, call `TelemetryHandle::current()` to get the handle for wake-event tracking. Use manual setup when you have multiple Tokio runtimes, don't own `main` (e.g., library code or embedded services), or need to integrate with existing runtime-building code.

### Using the `#[main]` macro

> **Note:** `#[dial9_tokio_telemetry::main]` is a **replacement** for `#[tokio::main]`, not a complement — do not use both on the same function. The macro builds and configures the Tokio runtime internally.

```rust,no_run
use dial9_tokio_telemetry::{main, config::{Dial9Config, Dial9ConfigBuilder}, telemetry::TelemetryHandle};

fn my_config() -> Dial9Config {
    Dial9ConfigBuilder::new(
        "/tmp/my_traces/trace.bin",
        1024 * 1024,      // rotate after 1 MiB per file
        5 * 1024 * 1024,  // keep at most 5 MiB on disk
    )
    .rotation_period(std::time::Duration::from_secs(300)) // optional: rotate every 5 min (default: 60 s)
    .with_runtime(|r| r.with_runtime_name("main").with_task_tracking(true))  // TracedRuntime knobs
    .with_tokio(|t| { t.worker_threads(4); }) // tokio knobs
    .build()
}

#[dial9_tokio_telemetry::main(config = my_config)]
async fn main() {
    // your async code here
    // `TelemetryHandle::current()` returns the per-thread handle for
    // spawning instrumented sub-tasks:
    let handle = TelemetryHandle::current();
    handle
        .spawn(async { /* wake events tracked */ })
        .await
        .unwrap();
}
```

The macro automatically spawns your function body as a task, so top-level code is visible in traces (unlike plain `#[tokio::main]` where `block_on` work is invisible — see [below](#the-root-future-is-not-instrumented)). dial9 installs a `TelemetryHandle` on every runtime-owned thread via `on_thread_start`. Call `TelemetryHandle::current()` to get it for spawning wake-tracked sub-tasks.

### Manual setup

```rust
use dial9_tokio_telemetry::telemetry::{RotatingWriter, TracedRuntime};

fn main() -> std::io::Result<()> {
    let writer = RotatingWriter::builder()
        .base_path("/tmp/my_traces/trace.bin")
        .max_file_size(100 * 1024 * 1024) // safety valve at 100 MiB per file
        .max_total_size(500 * 1024 * 1024) // keep at most 500 MiB on disk
        // .rotation_period(std::time::Duration::from_secs(300)) // optional: rotate every 5 min (default: 60 s)
        .build()?;

    let mut builder = tokio::runtime::Builder::new_multi_thread();
    builder.worker_threads(4).enable_all();

    let (runtime, guard) = TracedRuntime::build_and_start(builder, writer)?;
    let handle = guard.handle();

    runtime.block_on(async {
        handle.spawn(async {
            // your async code here will be instrumented
        }).await.unwrap();
    });

    Ok(())
}
```

Events are 6–16 bytes on the wire, and a typical request generates ~20–35 bytes of trace data (a few poll events plus park/unpark). At 10k requests/sec that's well under 1 MB/s — `RotatingWriter` caps total disk usage so you can leave it running indefinitely. Typical CPU overhead is under 5%.

Segments rotate on size _or_ time, whichever comes first. Time boundaries are wall-clock-aligned (e.g. a 60 s period rotates at the top of each minute), which produces clean S3 key paths when using the `worker-s3` feature.

## Can I use this in prod?

dial9-tokio-telemetry is designed for always-on production use, but it's still early software. Measure overhead and validate behavior in your environment before deploying to production.

## Is there a demo?

Yes, check out this [quick walkthrough (YouTube)](https://www.youtube.com/watch?v=zJOzU_6Mf7Q)!

The [viewer](https://dial9-tokio-telemetry.netlify.app/) (autodeployed from code in `main`) is hosted on Netlify for convenience. You can [load the demo trace](https://dial9-tokio-telemetry.netlify.app/?trace=demo-trace.bin) directly, or use [serve.py](/dial9-tokio-telemetry/serve.py) to run it locally (pure HTML and JS, client side only).

<img width="1288" height="659" alt="Screenshot 2026-03-01 at 3 52 59 PM" src="https://github.com/user-attachments/assets/77225801-70b1-4aef-b064-32bc2326b1ef" />

## Why dial9-tokio-telemetry?

It can be hard to understand application performance and behavior in async code. Dial9 tracks every event Tokio emits to create a detailed, micro-second-by-microsecond trace of your application behavior that you can analyze.

Compared to [tokio-console](https://github.com/tokio-rs/console), which is designed for live debugging, dial9-tokio-telemetry is designed for post-hoc analysis. Because traces are written to files with bounded disk usage, you can leave it running in production and come back later to deeply analyze what went wrong or why a specific request was slow. On Linux, traces include CPU profile samples and kernel scheduling events, so you can see not just _that_ a task was delayed but _what code_ was running on the worker instead.

## What gets recorded automatically

`TracedRuntime` installs hooks on the Tokio runtime. The following events are recorded out of the box:

| Event                            | Fields                                                                |
| -------------------------------- | --------------------------------------------------------------------- |
| `PollStart` / `PollEnd`          | timestamp, worker, task ID, spawn location, local queue depth         |
| `WorkerPark` / `WorkerUnpark`    | timestamp, worker, local queue depth, thread CPU time, schedstat wait |
| `QueueSample`                    | timestamp, global queue depth (sampled every 10 ms)                   |
| `TaskSpawn` / `SpawnLocationDef` | task→spawn-location mapping (when `task_tracking` is enabled)         |

## The root future is not instrumented

Tokio's runtime hooks only fire for _spawned_ tasks. The future you pass to `runtime.block_on(...)` is not a spawned task, so code that runs directly in it produces no `PollStart` / `PollEnd` events and is invisible to dial9. This includes everything at the top level of `#[tokio::main]`.

## Tracing span events (opt-in)

Enable the `tracing-layer` feature to record `tracing` span enter/exit events into the trace. This shows what happened inside each poll (e.g., which functions ran, how long each took, what fields they carried).

```rust,ignore
use dial9_tokio_telemetry::tracing_layer::Dial9TokioLayer;
use tracing_subscriber::prelude::*;

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
```

Filtering is strongly recommended. Libraries like the AWS SDK emit many internal spans that can produce over 100K events per second. The example above captures only spans from `my_app`. Each span enter+exit costs ~300ns total (~50-100ns is dial9 encoding overhead).

To make work visible, spawn it:

```rust,ignore
runtime.block_on(async {
    // Not instrumented — runs on the block_on root future.
    do_setup().await;

    // Instrumented — this task shows up in the trace.
    handle.spawn(async { do_real_work().await }).await.unwrap();
});
```

## Wake event tracking

To understand when Tokio itself is delaying your code (scheduler delay), you need to know when your future was _ready_ to run. Wake events — which task woke which other task — are _not_ captured automatically. Tokio's runtime hooks don't currently allow instrumenting wakes: capturing wakes requires wrapping the future. The simplest way is to use `handle.spawn` instead of `tokio::spawn`.

Use `handle.spawn()` instead of `tokio::spawn()`:

```rust,no_run
# use dial9_tokio_telemetry::telemetry::{RotatingWriter, TracedRuntime};
# fn main() -> std::io::Result<()> {
# let writer = RotatingWriter::new("/tmp/t.bin", 1024, 4096)?;
# let builder = tokio::runtime::Builder::new_multi_thread();
let (runtime, guard) = TracedRuntime::build_and_start(builder, writer)?;
let handle = guard.handle();

runtime.block_on(async {
    // wake events / scheduling delay captured
    handle.spawn(async { /* ... */ });

    // this task is still tracked, but won't have wake events
    tokio::spawn(async { /* ... */ });
});
# Ok(())
# }
```

For frameworks like Axum where you don't control the spawn call, you need to wrap the accept loop. See [`examples/metrics-service/src/axum_traced.rs`](/examples/metrics-service/src/axum_traced.rs) for a working example that wraps both the accept loop and per-connection futures.

## Custom events

You can emit your own application-level events into the trace alongside the built-in runtime events. Define a struct with `#[derive(TraceEvent)]` and call `record_event`:

```rust,no_run
# fn main() {
use dial9_trace_format::TraceEvent;
use dial9_tokio_telemetry::telemetry::{record_event, clock_monotonic_ns, TelemetryHandle};

#[derive(TraceEvent)]
struct RequestCompleted {
    #[traceevent(timestamp)]
    timestamp_ns: u64,
    status_code: u32,
    latency_us: u64,
    /// Optional fields use 1 byte on the wire when absent.
    error_message: Option<String>,
}

# let handle: TelemetryHandle = todo!();
record_event(
    RequestCompleted {
        timestamp_ns: clock_monotonic_ns(),
        status_code: 200,
        latency_us: 1500,
        error_message: None,
    },
    &handle,
);
# }
```

For events with repeated string values (HTTP methods, endpoint paths, etc.), implement `Encodable` manually to use string interning — see [`examples/custom_events.rs`](/dial9-tokio-telemetry/examples/custom_events.rs) for a complete example showing both patterns.

Custom events are encoded into the same thread-local buffer as built-in events (~100–200 ns per call) and appear in the trace viewer alongside poll/park/wake events.

## Platform support

Core telemetry (poll timing, park/unpark, queue depth, wake events) works on all platforms.

On Linux, you get additional data for free:

- **Thread CPU time** in park/unpark events via `CLOCK_THREAD_CPUTIME_ID` (vDSO, ~20–40 ns)
- **Scheduler wait time** via `/proc/self/task/<tid>/schedstat` — shows when the Tokio worker was not scheduled by the OS when it was ready.

On non-Linux platforms these fields are zero.

### CPU profiling (Linux only)

With the `cpu-profiling` feature, you can enable `perf_event_open`-based CPU sampling. This gives two key pieces of data:

1. Stack traces when code was running on the CPU (visualized as flamegraphs in the viewer)
2. Stack traces when the kernel _descheduled_ your thread, showing precisely where `std::thread::sleep`, `std::sync::Mutex` contention, or other blocking occurs in async code.

Both of these events are tied to the precise instant and thread that they happened on, so you can compare what was different between degraded and normal performance.

```rust,no_run
# #[cfg(feature = "cpu-profiling")]
# fn main() -> std::io::Result<()> {
# use dial9_tokio_telemetry::telemetry::{RotatingWriter, TracedRuntime};
use dial9_tokio_telemetry::telemetry::cpu_profile::{CpuProfilingConfig, SchedEventConfig};

# let writer = RotatingWriter::new("/tmp/t.bin", 1024, 4096)?;
# let builder = tokio::runtime::Builder::new_multi_thread();
let (runtime, guard) = TracedRuntime::builder()
    .with_task_tracking(true)
    .with_cpu_profiling(CpuProfilingConfig::default())
    .with_sched_events(SchedEventConfig::default().include_kernel(true))
    .with_trace_path("/tmp/t.bin")
    .build_and_start(builder, writer)?;
# Ok(())
# }
# #[cfg(not(feature = "cpu-profiling"))]
# fn main() {}
```

This pulls in [`dial9-perf-self-profile`](/perf-self-profile) for `perf_event_open` access. It records `CpuSample` events with raw stack frame addresses. When a `trace_path` is set, the background worker automatically symbolizes sealed segments (resolving addresses to function names via `/proc/self/maps` and blazesym) and gzip-compresses them on disk.

#### Requirements

**Frame pointers**: CPU profile stack traces rely on frame-pointer-based unwinding. Compile your application with frame pointers enabled, otherwise stack traces will be truncated or missing. Combine this with the required `tokio_unstable` flag:

```toml
# .cargo/config.toml
[build]
rustflags = ["--cfg", "tokio_unstable", "-C", "force-frame-pointers=yes"]
```

**`perf_event_paranoid`**: CPU profiling features require `perf_event_paranoid` ≤ 2 for sampling, and ≤ 1 for scheduler event tracking (`with_sched_events`):

```bash
# check current value
cat /proc/sys/kernel/perf_event_paranoid

# allow CPU sampling and scheduler event tracking
sudo sysctl kernel.perf_event_paranoid=1
```

**`kallsyms`**: Resolving kernel addresses requires `kptr_restrict == 0` for non-root, or else they will show up like: `[kernel] 0xffffffff81336901`:

```bash
# check current value
cat /proc/sys/kernel/kptr_restrict

# allow non-root to resolve kernel symbols
sudo sysctl kernel.kptr_restrict=0
```

#### Diagnosing long polls with CPU samples

Because CPU samples are tagged with the worker thread they were collected on, and the trace records which task is being polled on each worker at each instant, the viewer can correlate samples with individual polls. When a poll takes an unusually long time (a "long poll"), the CPU samples collected during that poll show you exactly what code was running — expensive serialization, accidental blocking I/O, lock contention, etc. In the trace viewer, click on a long poll to see its flamegraph, or shift+drag to aggregate CPU samples across a time range.

## Getting started

`TracedRuntime::build` returns a `(Runtime, TelemetryGuard)`. The guard owns the flush thread and provides a `TelemetryHandle` for enabling/disabling recording at runtime:

```rust,no_run
# use dial9_tokio_telemetry::telemetry::{RotatingWriter, TracedRuntime};
# fn main() -> std::io::Result<()> {
# let writer = RotatingWriter::new("/tmp/t.bin", 1024, 4096)?;
# let builder = tokio::runtime::Builder::new_multi_thread();
let (runtime, guard) = TracedRuntime::builder()
    .with_task_tracking(true)
    .build(builder, writer)?;

// start disabled, enable later
guard.enable();

// TelemetryHandle is Clone + Send — pass it around
let handle = guard.handle();
handle.disable();
# Ok(())
# }
```

### Multiple runtimes

For applications with multiple Tokio runtimes (e.g. thread-per-core, or separate request/IO runtimes), use `TelemetryCore` to create the telemetry session first, then attach each runtime:

```rust,no_run
# use dial9_tokio_telemetry::telemetry::{RotatingWriter, TelemetryCore};
# fn main() -> std::io::Result<()> {
# let writer = RotatingWriter::new("/tmp/t.bin", 1024, 4096)?;
let guard = TelemetryCore::builder()
    .writer(writer)
    .trace_path("/tmp/t.bin")
    .build()?;
guard.enable();

let mut main_builder = tokio::runtime::Builder::new_multi_thread();
main_builder.worker_threads(4).enable_all();
let (main_rt, main_handle) = guard.trace_runtime("main").build(main_builder)?;

let mut io_builder = tokio::runtime::Builder::new_multi_thread();
io_builder.worker_threads(2).enable_all();
let (io_rt, io_handle) = guard.trace_runtime("io").build(io_builder)?;

// Both runtimes share a single trace file with unique worker IDs.
// The trace viewer groups workers by runtime name.
// Use main_handle.spawn() / io_handle.spawn() for wake-tracked futures.
# Ok(())
# }
```

See [`examples/thread_per_core.rs`](/dial9-tokio-telemetry/examples/thread_per_core.rs) and [`examples/multi_runtime.rs`](/dial9-tokio-telemetry/examples/multi_runtime.rs) for complete examples.

**Shutdown**: Drop all runtimes before the `TelemetryGuard` so worker threads exit and flush their thread-local buffers. For a clean shutdown that waits for the background worker (e.g. S3 uploads) to drain, call `guard.graceful_shutdown(timeout)` instead of dropping the guard.

### Writers

`RotatingWriter` rotates files based on size and time, and evicts old ones to stay within a total size budget. By default, segments rotate every 60 seconds (wall-clock-aligned) or when they exceed `max_file_size`, whichever comes first. Time-based rotation produces clean segment boundaries (thread-local buffers are drained before sealing), so set `max_file_size` large enough that time-based rotation fires first under normal conditions (100 MiB is a good default). Size-based rotation then acts as a safety valve for unexpected data bursts. For quick experiments, use `RotatingWriter::single_file(path)` to skip rotation entirely.

### Analyzing traces

[`dial9-viewer`](/dial9-viewer) is an interactive trace viewer and S3 browser. Point it at a local directory or an S3 bucket to browse and visualize traces in the browser. [Here's a demo.](https://www.youtube.com/watch?v=zJOzU_6Mf7Q)

```bash
# Install
cargo install --locked dial9-viewer
# or, for pre-built binaries:
cargo binstall dial9-viewer

# Serve traces from a local directory
dial9-viewer serve --local-dir /tmp/my_traces

# Serve traces from S3
dial9-viewer serve --bucket my-trace-bucket
```

`dial9-viewer` also ships an agent toolkit (`dial9-viewer agents`) with skill documentation and JS analysis modules that AI agents can use to diagnose traces programmatically.

For CLI analysis without the viewer, there are example scripts:

```bash
# per-worker stats, wake→poll delays, idle worker detection
cargo run --example analyze_trace --features analysis -- /tmp/my_traces/trace.0.bin.gz

# convert to JSONL for ad-hoc scripting
cargo run --example trace_to_jsonl --features analysis -- /tmp/my_traces/trace.0.bin.gz output.jsonl
```

See [TRACE_ANALYSIS_GUIDE.md](/dial9-tokio-telemetry/TRACE_ANALYSIS_GUIDE.md) for a walkthrough of diagnosing scheduling delays and CPU hotspots from trace data.

## Features

- **`cpu-profiling`** — Linux only. Enables `perf_event_open`-based CPU sampling and scheduler event capture via `dial9-perf-self-profile`.
- **`worker-s3`** — Enables S3 upload support. Adds `aws-sdk-s3`, `aws-sdk-s3-transfer-manager`, `aws-config`, and `flate2`.

## S3 upload

With the `worker-s3` feature, sealed trace segments are automatically gzip-compressed and uploaded to S3 by a background worker thread. Application threads are unaffected: uploads happen on a background thread after segments are sealed.

Only `bucket` and `service_name` are required. See `S3Config` for additional options.

```rust,no_run
# #[cfg(feature = "worker-s3")]
# fn main() -> std::io::Result<()> {
use dial9_tokio_telemetry::telemetry::{RotatingWriter, TracedRuntime};
use dial9_tokio_telemetry::background_task::s3::S3Config;

let trace_path = "/tmp/my_traces/trace.bin";
let writer = RotatingWriter::builder()
    .base_path(trace_path)
    .max_file_size(100 * 1024 * 1024)  // safety valve at 100 MiB per file
    .max_total_size(500 * 1024 * 1024) // keep at most 500 MiB on disk
    .build()?;

let s3_config = S3Config::builder()
    .bucket("my-trace-bucket")
    .service_name("my-service")
    .build();

let mut builder = tokio::runtime::Builder::new_multi_thread();
builder.worker_threads(4).enable_all();

let (runtime, guard) = TracedRuntime::builder()
    .with_task_tracking(true)
    .with_trace_path(trace_path)
    .with_s3_uploader(s3_config)
    .build_and_start(builder, writer)?;

runtime.block_on(async {
    // your async code here
});

// guard drop: flushes, seals final segment, worker drains remaining to S3
# Ok(())
# }
# #[cfg(not(feature = "worker-s3"))]
# fn main() {}
```

By default (customizable via `S3Config::builder().key_fn(...)`), objects land at `s3://{bucket}/{prefix}/{YYYY-MM-DD}/{HHMM}/{service_name}/{instance_path}/{boot_id}/{epoch_secs}-{index}.bin.gz`. The time bucket is the first key component after the prefix, enabling efficient incident correlation: `aws s3 ls s3://bucket/traces/2026-03-07/2030/` lists all traces from all services during that minute. The `boot_id` (4 lowercase alpha chars by default, regenerated per process start) disambiguates segments across application restarts so segment indices from different runs never collide.

The worker requires `s3:PutObject` and `s3:HeadBucket` permissions.

The worker uses a circuit breaker with exponential backoff if S3 is unreachable. It never crashes or blocks the application. Segments remain on disk when uploads fail and are retried on the next poll cycle. For explicit shutdown control, use `guard.graceful_shutdown(timeout)` instead of dropping the guard (which seals the final segment but does not wait for the worker to drain).

## Examples

```bash
cargo run --example simple_workload        # macro-based setup (start here)
cargo run --example conditionally_enable   # toggle telemetry via ENABLE_DIAL9 env var
cargo run --example realistic_workload     # mixed CPU/IO workload
cargo run --example long_workload          # longer run for trace analysis
cargo run --example telemetry_rotating     # manual setup + rotating writer config
cargo run --example multi_runtime          # multiple runtimes, manual TelemetryCore
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

This repo is a Cargo workspace with five members:

- [`dial9-tokio-telemetry`](/dial9-tokio-telemetry) — the main crate
- [`dial9-viewer`](/dial9-viewer) — CLI and web UI for browsing traces in S3 or on the local filesystem
- [`dial9-macro`](/dial9-macro) — the `#[dial9_tokio_telemetry::main]` attribute macro
- [`dial9-perf-self-profile`](/perf-self-profile) — minimal Linux `perf_event_open` wrapper for CPU profiling and scheduler events
- [`examples/metrics-service`](/examples/metrics-service) — end-to-end example service

## Future work

- **Parquet output** — write traces as Parquet for efficient querying with Athena, DuckDB, etc.
- **Tokio task dumps** — capture async stack traces of all in-flight tasks
- **Retroactive sampling** — trace data lives in a ring buffer; when your application detects anomalous behavior, it triggers persistence of the last N seconds of data rather than recording everything continuously
- **Out-of-process symbolication** — resolve CPU profile stack traces in a background process to avoid adding latency or memory overhead to the application

## License

This project is licensed under the Apache-2.0 License.
