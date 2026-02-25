# Tokio Telemtry

The `telemetry` module records lightweight runtime telemetry — poll start/end, worker park/unpark, and queue depth samples — into a compact binary trace format. Traces can be analyzed offline to find idle workers, long polls, and scheduling imbalances.

### Quick Start

The easiest way to get started is with `TracedRuntime`, which wires up all the hooks and background threads for you. Use a `RotatingWriter` to bound disk usage in production (see [`examples/telemetry_rotating.rs`](examples/telemetry_rotating.rs) for the full example):

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

    // build_and_start() enables telemetry immediately
    // use build() to start disabled, then call guard.enable() later
    let (runtime, _guard) = TracedRuntime::build_and_start(builder, Box::new(writer))?;

    runtime.block_on(async {
        // ... your async code here ...
    });

    // Dropping `runtime` then `_guard` performs a final flush.
    Ok(())
}
```

`TracedRuntime::build` returns a `TelemetryGuard` whose `handle()` method gives you a cheap, cloneable `TelemetryHandle` you can use to enable/disable recording at runtime.

### Writers

| Writer | Use case |
|--------|----------|
| `RotatingWriter` | Production — automatically rotates and evicts old files to stay within a total size budget |
| `SimpleBinaryWriter` | Quick experiments — writes a single trace file with no size management |
| `NullWriter` | Benchmarking — measures hook overhead without any I/O |

**Future**: S3 writer for direct cloud storage, or use existing log shipping (CWAgent, Firelens, etc.) to push trace files.

### Analyzing Traces

Use the included examples to inspect trace files:

```bash
# Print a summary with per-worker stats and idle-worker detection
cargo run --example analyze_trace -- /tmp/my_traces/trace.0.bin

# Convert a binary trace to JSONL for ad-hoc analysis
cargo run --example trace_to_jsonl -- /tmp/my_traces/trace.0.bin output.jsonl

# Open the interactive HTML viewer
open trace_viewer.html
# Then drag-and-drop a .bin file to visualize the timeline
```

**Future**: S3 writer for direct cloud storage, or use existing log shipping (CWAgent, Firelens, etc.) to push trace files.

## Examples

Run examples with:
```bash
# Long-poll detection
cargo run --example long_sleep
cargo run --example cancelled_task
cargo run --example completing_task

# Telemetry
cargo run --example telemetry_demo
cargo run --example telemetry_rotating
```

## Benchmarks

Run benchmarks with:
```bash
cargo bench
```

### Overhead Comparison

Compare baseline vs telemetry overhead:
```bash
./scripts/compare_overhead.sh [duration_secs]
```

This runs the `overhead_bench` in both modes and validates:
- Telemetry overhead is acceptable (< 10%)
- Trace bytes per request (20-35 bytes) - tracks total trace data generated per client request
- Bytes per trace event (6-12 bytes) - validates binary format efficiency

Example output:
```
=== Comparison ===
Baseline:   286794 req/s, p50=174.1µs, p99=280.6µs
Telemetry:  277626 req/s, p50=180.2µs, p99=289.3µs
Overhead:   3.2%

=== Trace Efficiency ===
Trace bytes/request:    25.56
Bytes/trace event:      6.39
Client requests/sec:    277682
```

## Configuration

The system uses `.cargo/config.toml` to enable the `tokio_unstable` flag required for task dumps.

## Dependencies

- `tokio` (with `taskdump` feature)
- `arc-swap` (for lock-free sentinel updates)
- `pin-project-lite` (for proper pinning in the future wrapper)
- `smallvec` (for efficient small vector storage)

## Useful links

- [Code Browser](https://code.amazon.com/packages/RustProfilingExperiments/)
