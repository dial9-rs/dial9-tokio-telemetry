# Instrumenting your app with dial9

*This content is extracted from the [dial9-tokio-telemetry README](https://github.com/dial9-rs/dial9-tokio-telemetry).*

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

There are two ways to set up dial9: the `#[main]` macro (recommended for most apps) or manual `TracedRuntime` setup. The macro handles the boilerplate of building the runtime and spawning your code as an instrumented task. Inside the body, call `TelemetryHandle::current()` to reach a handle for wake-event tracking. Use manual setup when you have multiple tokio runtimes, don't own main (e.g. library code, embedded services), or need to integrate into existing runtime-building code.

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
    // `TelemetryHandle::current()` reaches the per-thread handle for
    // spawning instrumented sub-tasks:
    let handle = TelemetryHandle::current();
    handle
        .spawn(async { /* wake events tracked */ })
        .await
        .unwrap();
}
```

The macro automatically spawns your function body as a task, so top-level code is visible in traces (unlike plain `#[tokio::main]` where `block_on` work is invisible — see [below](#the-root-future-is-not-instrumented)). On every runtime-owned thread, dial9 installs a `TelemetryHandle` via `on_thread_start` — `TelemetryHandle::current()` returns it for spawning sub-tasks with wake-event tracking.

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

To understand when Tokio itself is delaying your code, generally referred to as scheduler delay, you need to know when your future was _ready_ to run. Wake events — which task woke which other task — are _not_ captured automatically. Tokio's runtime hooks don't currently allow instrumenting wakes: capturing wakes requires wrapping the future. The simplest way to do that is by using `handle.spawn` instead of `task::spawn`.

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
