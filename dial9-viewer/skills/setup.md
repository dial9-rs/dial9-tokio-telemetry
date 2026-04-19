# Instrumenting Your App

## 1. Add the dependency

```toml
[dependencies]
dial9-tokio-telemetry = { version = "0.3", features = ["tracing-layer"] }
tracing = "0.1"
tracing-subscriber = { version = "0.3", features = ["env-filter"] }
```

If using a path dependency (e.g., local checkout):

```toml
dial9-tokio-telemetry = { path = "/path/to/dial9-tokio-telemetry", features = ["tracing-layer"] }
```

## 2. Set up the runtime

### Option A: `#[main]` macro (simplest)

```rust,ignore
use dial9_tokio_telemetry::config::Dial9Config;
use dial9_tokio_telemetry::tracing_layer::Dial9TokioLayer;
use tracing_subscriber::prelude::*;

#[dial9_tokio_telemetry::main(config = my_config)]
async fn main() {
    // your app code
}

fn my_config() -> Dial9Config {
    Dial9Config::builder("trace.bin", 5_000_000, 50_000_000).build()
}
```

You still need to set up the tracing subscriber separately (before or inside main):

```rust,ignore
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

### Option B: Manual setup

### Option B: Manual setup

Replace your `tokio::runtime::Builder` with `TracedRuntime` and install `Dial9TokioLayer`:

```rust,ignore
use dial9_tokio_telemetry::telemetry::{RotatingWriter, TracedRuntime};
use dial9_tokio_telemetry::tracing_layer::Dial9TokioLayer;
use tracing_subscriber::prelude::*;

// Set up the tracing subscriber with the dial9 layer.
// Filter to your app's target to avoid noise from third-party crates.
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

## 3. Add `#[instrument]` to functions

```rust,ignore
#[tracing::instrument(skip_all, fields(request_id = %id))]
async fn handle_request(id: String, db: &Db) {
    // span enter/exit events are recorded automatically
    do_work(db).await;
}

#[tracing::instrument(skip_all)]
async fn do_work(db: &Db) {
    // nested spans show up as children in the trace
}
```

Use `skip_all` and explicitly name the fields you want. This avoids `Debug` formatting issues and keeps the trace compact.

## 4. Generate and analyze a trace

Run your app under load, then analyze:

```bash
dial9-viewer agents toolkit /tmp/d9-toolkit
node /tmp/d9-toolkit/analyze.js trace.0.bin
```

## Filtering

Libraries like the AWS SDK emit many internal spans (100K+ events/sec). Always filter the `Dial9TokioLayer` to your app's target:

```rust,ignore
Dial9TokioLayer::new().with_filter(
    tracing_subscriber::filter::Targets::new()
        .with_target("my_app", tracing::Level::TRACE)
        .with_default(tracing::Level::ERROR),
)
```

This only affects the dial9 layer. Your `fmt` layer still logs everything.

## Existing tracing subscriber

If your app already has a tracing subscriber, compose the dial9 layer with it:

```rust,ignore
// If you currently have:
//   tracing_subscriber::fmt().init();
// Replace with:
tracing_subscriber::registry()
    .with(tracing_subscriber::fmt::layer())
    .with(Dial9TokioLayer::new().with_filter(/* ... */))
    .init();
```

## Current-thread runtime

Works the same way. Replace `new_multi_thread()` with `new_current_thread()`:

```rust,ignore
let mut builder = tokio::runtime::Builder::new_current_thread();
builder.enable_all();
let (runtime, guard) = TracedRuntime::build_and_start(builder, writer)?;
```
