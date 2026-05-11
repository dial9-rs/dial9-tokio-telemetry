# dial9

[![Crates.io](https://img.shields.io/crates/v/dial9-tokio-telemetry.svg)](https://crates.io/crates/dial9-tokio-telemetry)
[![Documentation](https://docs.rs/dial9-tokio-telemetry/badge.svg)](https://docs.rs/dial9-tokio-telemetry)
![License](https://img.shields.io/crates/l/dial9-tokio-telemetry.svg)

dial9 is a microscope for Tokio (and Rust applications in general). It allows you to record a large number of events very cheaply and analyze them later. By incorporating data from Tokio, the operating system, and your application, hard-to-debug problems can become obvious. "What is Tokio actually doing?" becomes readily apparent.

[Demo (Youtube)](https://www.youtube.com/watch?v=kr0RYMu57kU) | [Demo Application](https://dial9-tokio-telemetry.netlify.app/?trace=demo-trace.bin) 

<img width="1288" height="659" alt="Screenshot 2026-03-01 at 3 52 59 PM" src="https://github.com/user-attachments/assets/77225801-70b1-4aef-b064-32bc2326b1ef" href="https://dial9-tokio-telemetry.netlify.app/?trace=demo-trace.bin" />

## Quick Start

dial9 is fundamentally an efficient way to collect data from [different sources](#data-sources) then [export them out of the application](#getting-data-out-of-dial9). You can enable as many different data sources as you need to debug (or as few as you can tolerate the overhead of in production.)

Once you have data, you will want to analyze it. There are two complementary paths:
1. The `dial9` crate which provides an HTML static site which can view the trace files. The viewer is also hosted [here](https://dial9-tokio-telemetry.netlify.app/).
2. Via the agent toolkit: `dial9` ships skill documentation and scripts to allow agents to perform scripted analysis of dial9 traces.

For more information see [Analyzing Trace Files](#analyzing-trace-files)

If you are integrating dial9 into a production service, see the [`production_use` example](https://github.com/dial9-rs/dial9-tokio-telemetry/blob/main/dial9-tokio-telemetry/examples/production_use.rs).

You can also find a full [example service](https://github.com/dial9-rs/dial9-tokio-telemetry/blob/main/examples).

Tokio relies on `tokio_unstable` for Tokio runtime hooks and frame pointers for efficient profiling.

```toml
# .cargo/config.toml
[build]
rustflags = [
  "--cfg", "tokio_unstable",
  # For profiling, you also need:
  "-C", "force-frame-pointers=yes"
]
```

```rust,no_run
use dial9_tokio_telemetry::{main, Dial9Config, telemetry::TelemetryHandle};

fn my_config() -> Dial9Config {
    Dial9Config::builder()
        .base_path("/tmp/my_traces/trace.bin")
        .max_file_size(1024 * 1024)        // rotate after 1 MiB per file
        .max_total_size(5 * 1024 * 1024)   // keep at most 5 MiB on disk
        .rotation_period(std::time::Duration::from_secs(300)) // optional: rotate every 5 min (default: 60 s)
        .with_runtime(|r| r.with_runtime_name("main").with_task_tracking(true))  // TracedRuntime knobs
        .with_tokio(|t| { t.worker_threads(4); }) // tokio knobs
        .build_or_disabled() // or use build() to handle config failures explicitly
}

#[dial9_tokio_telemetry::main(config = my_config)] // inline config function is also supported
async fn main() {
    let handle = TelemetryHandle::current();
    handle
        .spawn(async { /* wake events tracked */ })
        .await
        .unwrap();
}
```

## Why dial9-tokio-telemetry?

It can be hard to understand application performance and behavior in async code. dial9 tracks Tokio, operating system and application events to create a detailed, nanosecond-by-nanosecond trace of your application behavior that you can analyze. On Linux, traces include CPU profile samples and kernel scheduling events, so you can see not just _that_ a task was delayed but _what code_ was running on the worker instead.

Compared to [tokio-console](https://github.com/tokio-rs/console), which is designed for live debugging, dial9-tokio-telemetry is designed for post-hoc analysis and to be a tool you can run in production. dial9 pushes out trace files to disk, S3 and anywhere else you configure. After a problem happens, you can come back to the trace to figure out the problem.

Compared to [tokio-metrics](https://github.com/tokio-rs/tokio-metrics), which exports aggregate counters (mean poll time, queue depth, etc.) for dashboarding and alerting, dial9 records every individual event. tokio-metrics can tell you something is wrong. dial9 can tell you _what_ is wrong. Use tokio-metrics for operational dashboards, and dial9 for debugging the root cause.

## Data sources

dial9 is fundamentally a central buffer that can collect data from different sources. You can pull in as many or as few as you want.

- [Tokio Events](#tokio-events): dial9 can capture poll, wake, and worker events from Tokio
- [CPU profiling](#cpu-profiling-linux-only): dial9 can capture linux performance counters and events to produce flamegraphs
- [Tracing spans](#tracing-span-events-opt-in): dial9 can capture tracing spans to bring tracing context into your trace files
- [Task dumps](#task-dumps-linux-only): dial9 can capture a task dump (a backtrace when your future goes idle) to determine what it is waiting for when idle
- [Custom events](#custom-events): dial9 can record custom application events into the trace


### Tokio events
`dial9` uses Tokio runtime hooks to record events on each `poll`, task `spawn` and when runtime workers park and unpark. If you use `dial9`'s [`spawn`](https://docs.rs/dial9-tokio-telemetry/latest/dial9_tokio_telemetry/telemetry/fn.spawn.html) your future will be instrumented to capture two additional pieces of info:
1. The wake event, when your future was _ready_ to run vs. when Tokio actually started running it.
2. A "task dump", a stack trace of what your future was doing when it went idle.

`dial9` can instrument a single runtime by using `TracedRuntime` or by using the `dial9_tokio_telemetry::main` macro.

```rust
# #[cfg(feature = "worker-s3")]
# mod inner {
use dial9_tokio_telemetry::Dial9Config;
use dial9_tokio_telemetry::background_task::s3::S3Config;

fn my_config() -> Dial9Config {
    let s3_config = S3Config::builder()
        .bucket("my-trace-bucket")
        .service_name("my-service")
        .build();

    Dial9Config::builder()
        .base_path("/tmp/my_traces/trace.bin")
        .max_file_size(100 * 1024 * 1024)
        .max_total_size(500 * 1024 * 1024)
        .with_tokio(|t| { t.worker_threads(4); })
        .with_runtime(|r| {
            r.with_task_tracking(true)
             .with_s3_uploader(s3_config)
        })
        .build_or_disabled()
}
```

#### Instrumenting multiple runtimes

`dial9` can also capture data from multiple runtimes. 
See [`examples/thread_per_core.rs`](https://github.com/dial9-rs/dial9-tokio-telemetry/blob/main/dial9-tokio-telemetry/examples/thread_per_core.rs) and [`examples/multi_runtime.rs`](https://github.com/dial9-rs/dial9-tokio-telemetry/blob/main/dial9-tokio-telemetry/examples/multi_runtime.rs) for complete examples.

### CPU profiling (Linux only)

dial9 supports two forms of CPU profiling:
- "traditional" CPU profiling / flamegraphs: dial9 can use Linux perf events with a fallback to `ctimer` for containerized environments. This allows you to get application stacks with attached metadata. You can see exactly what was happening during a long poll or see a flamegraph for one specific Tokio task.
- schedule profiling: With `perf_event_paranoid <= 1` dial9 can capture stack traces when your code is moved off-CPU by the kernel. This is extremely helpful when diagnosing issues in async applications: If your future is moved off CPU while polling this is almost always an indication of a problem.

Both of these events are tied to the precise instant and thread that they happened on, so you can compare what was different between degraded and normal performance.

#### Application Requirements

**Enable the `cpu-profiling` feature**:
```toml
[dependencies]
dial9-tokio-telemetry = { version = "0.3", features = ["cpu-profile"] }
```

**Enable frame pointers**:
```toml
# .cargo/config.toml
[build]
rustflags = ["--cfg", "tokio_unstable", "-C", "force-frame-pointers=yes"]
```

**Set `with_cpu_profiling`**:

```rust,ignore
use dial9_tokio_telemetry::Dial9Config;
use dial9_tokio_telemetry::telemetry::cpu_profile::{CpuProfilingConfig, SchedEventConfig};
Dial9Config::builder()
    // ...
    .with_runtime(|r| {
        // Enable normal CPU profiles
        .with_cpu_profiling(CpuProfilingConfig::default())
        // Enable scheduling profiling
        .with_sched_events(SchedEventConfig::default().include_kernel(true))
    })
    // ...
```

#### System requirements
- `perf_event_paranoid`: CPU profiling requires <= 2. `sched_events` requires <= 1.
    ```bash
    # check current value
    cat /proc/sys/kernel/perf_event_paranoid
    
    # allow CPU sampling and scheduler event tracking
    sudo sysctl kernel.perf_event_paranoid=1
    ```

- Kernel stack traces: To enable dial9 to symbolize traces that go into kernel functions `kernel.kptr_restrict` must be 0 for non-root, or else they will show up like: `[kernel] 0xffffffff81336901`:
  ```bash
  sudo sysctl kernel.kptr_restrict=0
  ```

### Tracing span events (opt-in)

**Enable the `tracing-layer` feature:**
```toml
[dependencies]
dial9-tokio-telemetry = { version = "0.3", features = ["tracing-layer"] }
```

**Use tracing_subscriber to connect the `Dial9TokioLayer`:**
```rust
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

Careful filtering of the data you send to dial9 strongly recommended. dial9 doesn't need _all_ the data, only enough to correlate with other data sources. Libraries like the AWS SDK emit many internal spans that can produce over 100K events per second. The example above captures only spans from my_app. Each span enter+exit costs ~300ns total (~50-100ns is dial9 encoding overhead).


### Task dumps (Linux only)

`dial9` can capture async backtraces at yield points. This is the Tokio equivalent of scheduling events: You can see the stack trace your future was at when it went idle.

> Note: The taskdump feature requires Tokio's upstream taskdump support, which only compiles on Linux (aarch64, x86, x86_64). Enabling it on other targets is a hard compile error from Tokio.

```rust
# #[cfg(feature = "taskdump")]
# mod inner {
# use std::time::Duration;
use dial9_tokio_telemetry::{Dial9Config, telemetry::TaskDumpConfig};

fn my_config() -> Dial9Config {
    Dial9Config::builder()
        // ...
        .with_runtime(|r| {
            r.with_task_tracking(true)
             .with_task_dumps(TaskDumpConfig::builder().idle_threshold(Duration::from_millis(10)).build())
        })
        .build_or_disabled()
}

#[dial9_tokio_telemetry::main(config = my_config)]
async fn main() { /* ... */ }
# }
# fn main() {}
```

> Performance note: Task dumps currently produce one extra wake per capture and are more likely than other features to degrade performance. Measure overhead in your environment before enabling in latency-sensitive paths.

### Custom events

You can emit your own application-level events into the trace alongside the built-in runtime events. Define a struct with `#[derive(TraceEvent)]` and call `record_event`:

```rust
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

## Getting data out of dial9

dial9 is recording data to in memory buffers and eventually to disk. For most applications, they would like the data to go somewhere else. `dial9` has a built in exporter for S3 and it is also possible to write your own exporter.

### Exporting data to S3
dial9 has a built-in S3 exporter. When segments are sealed, symbolized, and compressed they will be uploaded to S3 by a background thread.

**Enable the `worker-s3` feature:**
```toml
[dependencies]
dial9-tokio-telemetry = { version = "0.3", features = ["worker-s3"] }
```

**Create the S3 bucket**: Ensure your application has `s3:PutObject` and `s3:ListBucket` permissions to the bucket.

**Set `with_s3_uploader`:**
```rust,no_run
# #[cfg(feature = "worker-s3")]
# mod inner {
use dial9_tokio_telemetry::Dial9Config;
use dial9_tokio_telemetry::background_task::s3::S3Config;

fn my_config() -> Dial9Config {
    let s3_config = S3Config::builder()
        .bucket("my-trace-bucket")
        .service_name("my-service")
        .build();

    Dial9Config::builder()
        // ...
        .with_runtime(|r| {
            r.with_task_tracking(true)
             .with_s3_uploader(s3_config)
        })
        .build_or_disabled()
}

#[dial9_tokio_telemetry::main(config = my_config)]
async fn main() {
    // your async code here
}
// on shutdown: flushes, seals final segment, worker drains remaining to S3
# }
# fn main() {}
```

By default objects will be uploaded to `s3://{bucket}/{prefix}/{YYYY-MM-DD}/{HHMM}/{service_name}/{instance_path}/{boot_id}/{epoch_secs}-{index}.bin.gz`. You can customize the location with `S3Config::builder().key_fn(...)`.

The worker uses a circuit breaker with exponential backoff if S3 is unreachable. It never crashes or blocks the application. Segments remain on disk when uploads fail and are retried on the next cycle. To ensure the last segment is uploaded, use `guard.graceful_shutdown(timeout)` instead of dropping the guard (which seals the final segment but does not wait for the worker to drain).

### Exporting data to other destinations

For custom upload destinations or post-processing (e.g. shipping to a different object store, running analysis on each segment), you can replace the built-in pipeline entirely with `with_custom_pipeline`. See [`examples/custom_pipeline.rs`](https://github.com/dial9-rs/dial9-tokio-telemetry/blob/main/dial9-tokio-telemetry/examples/custom_pipeline.rs) for a complete example.

## Analyzing trace files
[`dial9`](https://github.com/dial9-rs/dial9-tokio-telemetry/tree/main/dial9-viewer) is a CLI for browsing and analyzing traces. Use `dial9 serve` to start a local web UI that visualizes traces from a directory or S3 bucket. [Here's a demo.](https://www.youtube.com/watch?v=kr0RYMu57kU)

```bash
# Install
cargo install --locked dial9
# or, for pre-built binaries:
cargo binstall dial9

# Serve traces from a local directory
dial9 serve --local-dir /tmp/my_traces

# Serve traces from S3
dial9 serve --bucket my-trace-bucket
```

### Agent toolkit

`dial9` also ships skill documentation and JS analysis modules for scripted trace analysis.

```bash
# Print the agent skill overview
dial9 agents

# Unpack all skills to a directory
dial9 agents skills /path/to/skills

# Extract the JS analysis toolkit
dial9 agents toolkit /path/to/toolkit
node /path/to/toolkit/analyze.js /tmp/my_traces/
```

If you use [Symposium](https://symposium.dev), skills auto-install when your project depends on `dial9-tokio-telemetry`:

```bash
cargo agents sync
```

## License

This project is licensed under the Apache-2.0 License.