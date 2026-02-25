# Metrics Service Example

A demonstration service that collects, aggregates, and stores metrics using Tokio, Axum, and DynamoDB, instrumented with `dial9-tokio-telemetry` for runtime tracing.

## What It Does

- **HTTP API**: Accepts metric submissions via POST and queries aggregated metrics via GET
- **In-memory buffering**: Collects metrics in memory before periodic flushing
- **DynamoDB persistence**: Stores aggregated metrics with timestamp-based partitioning
- **Load testing**: Built-in client that simulates variable load patterns
- **Telemetry**: Captures Tokio runtime traces to disk for performance analysis

The service runs for 55 seconds with a load profile that ramps up, sustains, ramps down, and includes a thundering herd spike.

## Usage

Run the example:

```bash
cargo run
```

The service will:
1. Start an HTTP server on `0.0.0.0:3001`
2. Create a DynamoDB table named `metrics-service` (requires AWS credentials)
3. Launch a background flush worker (10-second intervals)
4. Run a load-generating client with varying concurrency
5. Write telemetry traces to `/tmp/metrics-service-traces/`
6. Shut down automatically after 55 seconds

### API Endpoints

**Record a metric:**
```bash
curl -X POST http://localhost:3001/metrics \
  -H "Content-Type: application/json" \
  -d '{"name": "cpu", "value": 42.5}'
```

**Query aggregated metrics:**
```bash
curl http://localhost:3001/metrics/cpu
```

Returns JSON array with timestamp, sum, count, min, max for each time window.

## Configuration

Edit constants in `src/main.rs`:

| Constant | Default | Description |
|----------|---------|-------------|
| `FLUSH_INTERVAL` | 10s | How often to flush buffered metrics to DynamoDB |
| `TABLE_NAME` | `"metrics-service"` | DynamoDB table name |
| `SERVER_ADDR` | `"0.0.0.0:3001"` | HTTP server bind address |
| `RUN_DURATION` | 55s | Total runtime before shutdown |

### Load Profile

Edit `src/client.rs` to adjust:
- `MAX_WORKERS`: Peak concurrent requests (default: 40)
- `THUNDERING_HERD`: Spike concurrency (default: 200)
- `BASELINE`: Steady-state concurrency (default: 4)
- `METRICS`: Metric names to cycle through

### Telemetry

Traces are written to `/tmp/metrics-service-traces/trace.bin` with:
- Max file size: 1 MB (rotates automatically)
- Max total size: 30 MB

Change the path or limits in the `RotatingWriter::new()` call in `main.rs`.

## Requirements

- AWS credentials configured (for DynamoDB access)
- Write permissions to `/tmp/metrics-service-traces/`
