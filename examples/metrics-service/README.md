# Metrics Service Example

A small Axum service that accepts, buffers, and flushes metrics to DynamoDB—instrumented with `dial9-tokio-telemetry` for runtime tracing. A built-in load-generating client drives the service through a variable concurrency profile (ramp → thundering herd → baseline) so you get interesting traces to look at.

## Running

```bash
cargo run
```

This starts the HTTP server, creates a DynamoDB table, spawns the load client as a subprocess, and shuts everything down after 55 seconds. Traces are written to `/tmp/metrics-service-traces/trace.bin`. You need AWS credentials configured for DynamoDB access.

The server and client are separate binaries. The main binary spawns the client automatically, but you can also run them independently:

```bash
# in one terminal
cargo run --bin metrics-service

# in another
cargo run --bin client -- --server-url http://127.0.0.1:3001
```

## API

```bash
# record a metric
curl -X POST http://localhost:3001/metrics \
  -H "Content-Type: application/json" \
  -d '{"name": "cpu", "value": 42.5}'

# query aggregated metrics
curl http://localhost:3001/metrics/cpu
```

## Configuration

Everything is controlled via CLI flags—run `cargo run -- --help` for the full list. The load profile constants (`MAX_WORKERS`, `THUNDERING_HERD`, `BASELINE`, etc.) live at the top of `src/client.rs`.

On Linux, CPU profiling and scheduler event tracing are enabled automatically.
