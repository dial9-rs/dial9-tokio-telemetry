# dial9-tokio-telemetry

[![Crates.io](https://img.shields.io/crates/v/dial9-tokio-telemetry.svg)](https://crates.io/crates/dial9-tokio-telemetry)
[![Documentation](https://docs.rs/dial9-tokio-telemetry/badge.svg)](https://docs.rs/dial9-tokio-telemetry)
[![License](https://img.shields.io/crates/l/dial9-tokio-telemetry.svg)]

**Low-overhead runtime telemetry for Tokio.** Records poll timing, worker park/unpark, wake events, queue depths, and (on Linux) CPU profile samples into a compact binary trace format. Traces can be analyzed offline to find long polls, scheduling delays, idle workers, and CPU hotspots.

## Prerequisites

This crate requires Tokio's unstable APIs for runtime hooks and worker metrics. Add the following to your project's `.cargo/config.toml`:

```toml
# .cargo/config.toml
[build]
rustflags = ["--cfg", "tokio_unstable"]
```

Without this flag, compilation will fail with errors about missing methods on `tokio::runtime::Builder` and `RuntimeMetrics`.

## Quick start

```rust
use dial9_tokio_telemetry::telemetry::{RotatingWriter, TracedRuntime};

fn main() -> std::io::Result<()> {
    let writer = RotatingWriter::builder()
        .base_path("/tmp/my_traces/trace.bin")
        .max_file_size(1024 * 1024)      // rotate after 1 MiB per file
        .max_total_size(5 * 1024 * 1024) // keep at most 5 MiB on disk
        .build()?;

    let mut builder = tokio::runtime::Builder::new_multi_thread();
    builder.worker_threads(4).enable_all();

    let (runtime, _guard) = TracedRuntime::build_and_start(builder, writer)?;

    runtime.block_on(async {
        // your async code here
    });

    Ok(())
}
```

Events are 6–16 bytes on the wire, and a typical request generates ~20–35 bytes of trace data (a few poll events plus park/unpark). At 10k requests/sec that's well under 1 MB/s — `RotatingWriter` caps total disk usage so you can leave it running indefinitely. Typical CPU overhead is under 5%.

## Can I use this in prod?
dial9-tokio-telemetry is designed for always-on production use, but it's still early software. Measure overhead and validate behavior in your environment before deploying to production.

## Is there a demo?
Yes, checkout this [quick walkthrough (YouTube)](https://www.youtube.com/watch?v=zJOzU_6Mf7Q)!

The [viewer](https://dial9-tokio-telemetry.russell-r-cohen.workers.dev/) (autodeployed from code in `main`) is hosted on cloudflare pages for convenience. You can [load the demo trace](https://dial9-tokio-telemetry.russell-r-cohen.workers.dev/?trace=demo-trace.bin) directly, or use [serve.py](/dial9-tokio-telemetry/serve.py) to run it locally (pure HTML and JS, client side only).

<img width="1288" height="659" alt="Screenshot 2026-03-01 at 3 52 59 PM" src="https://github.com/user-attachments/assets/77225801-70b1-4aef-b064-32bc2326b1ef" />


## Why dial9-tokio-telemetry?

It can be hard to understand application performance and behavior, in async code. Dial9 tracks every event Tokio emits to create a detailed, micro-second-by-microsecond trace of your application behavior that you can analyze.

Compared to [tokio-console](https://github.com/tokio-rs/console), which is designed for live debugging, dial9-tokio-telemetry is designed for post-hoc analysis. Because traces are written to files with bounded disk usage, you can leave it running in production and come back later to deeply analyze what went wrong or why a specific request was slow. On Linux, traces include CPU profile samples and kernel scheduling events, so you can see not just *that* a task was delayed but *what code* was running on the worker instead.

## What gets recorded automatically

`TracedRuntime` installs hooks on the Tokio runtime. The following events are recorded out of the box:

| Event | Fields |
|-------|--------|
| `PollStart` / `PollEnd` | timestamp, worker, task ID, spawn location, local queue depth |
| `WorkerPark` / `WorkerUnpark` | timestamp, worker, local queue depth, thread CPU time, schedstat wait |
| `QueueSample` | timestamp, global queue depth (sampled every 10 ms) |
| `TaskSpawn` / `SpawnLocationDef` | task→spawn-location mapping (when `task_tracking` is enabled) |

## Wake event tracking

To understand when Tokio itself is delaying your code, generally referred to as scheduler delay, you need to know when your future was _ready_ to run. Wake events — which task woke which other task — are *not* captured automatically. Tokio's runtime hooks don't currently allow instrumenting wakes: capturing wakes requires wrapping the future. The simplest way to do that is by using `handle.spawn` instead of `task::spawn`. 

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

## Platform support

Core telemetry (poll timing, park/unpark, queue depth, wake events) works on all platforms.

On Linux, you get additional data for free:
- **Thread CPU time** in park/unpark events via `CLOCK_THREAD_CPUTIME_ID` (vDSO, ~20–40 ns)
- **Scheduler wait time** via `/proc/self/task/<tid>/schedstat` — shows when the Tokio worker was not scheduled by the OS when it was ready.

On non-Linux platforms these fields are zero.

### CPU profiling (Linux only)

With the `cpu-profiling` feature, you can enable `perf_event_open`-based CPU sampling. This gives two key pieces of data:
1. Stack traces when code was running on the CPU — aka flamegraphs
2. Stack traces when the kernel _descheduled_ your thread. For example, if you use `std::thread::sleep` in your future or are seeing `std::sync::Mutex` contention, this will allow you to see precisely where this is happening in async code.

Both of these events are tied to the precise instant and thread that they happened on, so you can compare what was different between degraded and normal performance.

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

### Writers

`RotatingWriter` rotates files and evicts old ones to stay within a total size budget. For quick experiments, `RotatingWriter::single_file(path)` writes a single file with no rotation.

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
- **`worker-s3`** — Enables S3 upload support. Adds `aws-sdk-s3`, `aws-sdk-s3-transfer-manager`, `aws-config`, and `flate2`.

## S3 upload

With the `worker-s3` feature, sealed trace segments are automatically gzip-compressed and uploaded to S3 by a background worker thread. The application process is unaffected: uploads happen asynchronously after segments are sealed.

Only `bucket` and `service_name` are required. See [`S3Config`](https://docs.rs/dial9-tokio-telemetry/latest/dial9_tokio_telemetry/background_task/s3/struct.S3Config.html) for additional options.

```rust,no_run
# #[cfg(feature = "worker-s3")]
# fn main() -> std::io::Result<()> {
use dial9_tokio_telemetry::telemetry::{RotatingWriter, TracedRuntime};
use dial9_tokio_telemetry::background_task::s3::S3Config;

let trace_path = "/tmp/my_traces/trace.bin";
let writer = RotatingWriter::builder()
    .base_path(trace_path)
    .max_file_size(1024 * 1024)      // rotate after 1 MiB per file
    .max_total_size(5 * 1024 * 1024) // keep at most 5 MiB on disk
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

By default (customizable via `S3Config::builder().key_fn(...)`), objects land at `s3://{bucket}/{prefix}/{YYYY-MM-DD}/{HHMM}/{service_name}/{instance_path}/{epoch_secs}-{index}.bin.gz`. The time bucket is the first key component after the prefix, enabling efficient incident correlation: `aws s3 ls s3://bucket/traces/2026-03-07/2030/` lists all traces from all services during that minute.

The worker requires `s3:PutObject` and `s3:HeadBucket` permissions.

The worker uses a circuit breaker with exponential backoff if S3 is unreachable. It never crashes or blocks the application. Segments remain on disk when uploads fail and are retried on the next poll cycle. For explicit shutdown control, use `guard.graceful_shutdown(timeout)` instead of dropping the guard (which seals the final segment but does not wait for the worker to drain).

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

- **Parquet output** — write traces as Parquet for efficient querying with Athena, DuckDB, etc.
- **Tokio task dumps** — capture async stack traces of all in-flight tasks
- **Retroactive sampling** — trace data lives in a ring buffer; when your application detects anomalous behavior, it triggers persistence of the last N seconds of data rather than recording everything continuously
- **Out-of-process symbolication** — resolve CPU profile stack traces in a background process to avoid adding latency or memory overhead to the application

## License

This project is licensed under the Apache-2.0 License.
