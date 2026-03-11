# S3 Worker Design

## Overview

Get trace data from running processes into S3 with minimal in-process overhead. The application writes traces to local disk; a background worker uploads them asynchronously.

**Core principle:** Keep the hot path simple. Push all heavy work (S3 uploads, compression, retries) to a worker that can't affect application performance.

## Architecture

```
Application Process              Worker Thread (dedicated tokio current_thread runtime)
┌─────────────────┐             ┌──────────────────────────────────┐
│ TracedRuntime    │             │ WorkerLoop                       │
│   RotatingWriter │────────────▶│   ┌──────────────────────────┐  │
│   /tmp/traces/   │  .bin files │   │ SegmentProcessor pipeline │  │
│                  │             │   │  1. GzipCompressor        │  │
│                  │             │   │  2. S3PipelineUploader     │  │
│                  │             │   └──────────────────────────┘  │
└─────────────────┘             └──────────────────────────────────┘
                                           │
                                           ▼
                        S3: {bucket}/{prefix}/{date-time}/
                            {service}/{instance}/
                            {epoch_secs}-{index}.bin.gz
```

The worker runs on a dedicated OS thread with its own single-threaded tokio runtime (`tokio::runtime::Builder::new_current_thread`). This isolates it completely from the application's runtime — the worker can never steal time from application tasks.

## Key Design Decisions

### 1. Rename-on-seal for atomicity

**Problem:** How does the worker know when a segment is safe to read?

**Solution:** Write to `.bin.active`, rename to `.bin` when complete. Rename is atomic on Linux. Worker only processes `.bin` files.

```
Writing:  trace.3.bin.active
Sealed:   trace.3.bin          (atomic rename)
```

**Why not inotify/fswatch?** Adds complexity and platform-specific code. Polling every 1s is simple and sufficient.

### 2. Time-first S3 key layout

**Problem:** The primary access pattern is incident correlation — "what was happening across all services at time T?" This means time should be the first index in the key hierarchy.

**Decision:** Time (1-minute bucket) is the first component after the optional prefix:

```
{prefix}/{date-time}/{service}/{instance}/{epoch_secs}-{index}.bin.gz
```

Example: `traces/2026-03-07/2030/checkout-api/us-east-1/i-0abc123/1741384542-3.bin.gz`

**Why time-first instead of service-first?**

| Layout | Incident query ("what happened at 8:30pm?") | Single-service query |
|--------|------------------------------------------|---------------------|
| `{time}/{service}/...` | `ListObjects(prefix=traces/2026-03-07/2030/)` — one call, all services | `ListObjects(prefix=traces/2026-03-07/2030/checkout-api/)` — still one call |
| `{service}/{time}/...` | N calls, one per service — must know all service names upfront | `ListObjects(prefix=traces/checkout-api/2026-03-07/2030/)` — one call |

Time-first is strictly better for incident correlation and no worse for single-service queries. The only case where service-first wins is "list all time ranges for one service" — but that's a rare access pattern compared to "what happened during this incident."

**Benefits:**
- Time-range queries across all services with a single `ListObjectsV2` prefix
- Natural Athena partitioning if we add Parquet output later
- Efficient S3 lifecycle policies (delete everything older than N days)
- 1-minute bucketing gives 1440 prefixes per day — manageable for listing and lifecycle policies

**Tradeoff:** Requires reasonable clock sync, but we already need that for trace timestamps.

### 3. Gzip compression

**Problem:** Trace files are large (binary event streams).

**Solution:** Gzip in memory before upload. Trace data is highly compressible (repetitive structures). Compression runs via `tokio::task::spawn_blocking` to keep the worker's event loop responsive.

**Why gzip not zip?** Simpler, standard `Content-Encoding` header, better compression ratio.

**Why gzip and not zstd?** Zstd has better compression ratios and speed, but gzip has wider ecosystem support: S3 `Content-Encoding: gzip` is universally understood, every CLI tool can decompress it (`gunzip`, `zcat`), and the `flate2` crate is already a transitive dependency via the AWS SDK. Switching to zstd would add a native C dependency (`zstd-sys`) for marginal gains on files that are typically 1-5 MB. If compression becomes a bottleneck, zstd is a straightforward swap.

### 4. Circuit breaker

**Problem:** S3 outages shouldn't crash the worker or lose data.

**Disk space safety:** Running out of disk space is worse than losing trace data. `RotatingWriter` enforces a `max_total_size` budget — when total disk usage exceeds the limit, it deletes the oldest sealed segments. This means if S3 is unreachable and files accumulate, the writer evicts old segments to stay within bounds. Data loss is acceptable; disk exhaustion is not. The worker processes oldest-first to maximize the upload window before eviction.

**Solution:** `CircuitBreaker` enum tracks connection health with exponential backoff:

```rust
pub enum CircuitBreaker {
    Closed,                                    // Healthy — upload normally
    Open { next_retry: Instant, backoff: Duration },  // Degraded — skip until retry time
}
```

```
Closed (healthy): upload + delete
   ↓ (upload fails)
Open (degraded): skip uploads, keep files on disk, exponential backoff
   ↓ (retry timer expires + retry succeeds)
Closed
```

Backoff: 1s → 2s → 4s → ... → 5min cap. Success resets to Closed immediately.

**Evicted files don't trip the circuit breaker.** If a segment disappears (evicted by `RotatingWriter`) during processing, the worker logs at debug level and skips it — this is normal operation, not an S3 failure.

**Why not crash?** Compressed files on disk are still valuable. Can be manually uploaded or recovered when S3 comes back.

### 5. Segment metadata

**Problem:** Trace files in S3 have no context about where they came from.

**Solution:** Write `SegmentMetadata` event at start of each segment:

```rust
TelemetryEvent::SegmentMetadata {
    entries: vec![
        ("service", "checkout-api"),
        ("host", "i-0abc123"),
        ("boot_id", "a3f7c2d1-..."),
    ]
}
```

Also set S3 object metadata headers for quick inspection via `HeadObject` without downloading.

**Why both?** Trace file is authoritative (works offline). S3 headers are convenience for CLI/UI.

### 6. Feature flags

**Problem:** Not everyone needs S3 upload. AWS SDK is a heavy dependency.

**Solution:** Two tiers:

```toml
# Core only (no worker)
dial9-tokio-telemetry = "0.1"

# Worker with S3 upload
dial9-tokio-telemetry = { version = "0.1", features = ["worker-s3"] }
```

`worker-s3` pulls in `aws-sdk-s3`, `aws-sdk-s3-transfer-manager`, `aws-config`, `flate2`, `time`.

### 7. Processor pipeline

**Problem:** The worker needs to do multiple things to each segment (compress, upload), and we want to add more steps later (symbolization, format conversion).

**Solution:** A `SegmentProcessor` trait with a pipeline of processors:

```rust
pub(crate) trait SegmentProcessor: Send {
    fn name(&self) -> &'static str;
    fn process(&mut self, data: SegmentData)
        -> Pin<Box<dyn Future<Output = Result<SegmentData, ProcessError>> + Send + '_>>;
}
```

`SegmentData` flows through the pipeline, carrying the segment bytes, accumulated metadata, and a metrics guard. Each processor transforms the data and passes it to the next. On error, `ProcessError` carries the `SegmentData` back so metrics are still recorded.

Current pipeline: `GzipCompressor` → `S3PipelineUploader`.

**Why a trait instead of hardcoded steps?** Extensibility for symbolization, format conversion, and testing (swap in mock processors). The trait uses manual boxed futures rather than `async_trait` to avoid the dependency.

### 8. Region auto-detection

**Problem:** Users shouldn't need to know which region their S3 bucket is in.

**Solution:** On startup, the worker calls `HeadBucket` to detect the bucket's region. If the call fails (e.g. wrong-region 301), the `x-amz-bucket-region` response header provides the correct region. The worker rebuilds the S3 client with the detected region.

Falls back to `us-east-1` if detection fails entirely.

### 9. Custom S3 key layout

**Problem:** Some users need a different S3 key structure.

**Solution:** `S3Config` accepts an optional `key_fn` implementing the `S3KeyFn` trait:

```rust
pub trait S3KeyFn: Send + Sync {
    fn object_key(&self, segment: &SealedSegment, metadata: &HashMap<String, String>) -> String;
}
```

When set, it completely overrides the default time-first key layout. Closures implement `S3KeyFn` automatically.

## API

```rust
use dial9_tokio_telemetry::telemetry::{RotatingWriter, TracedRuntime};
use dial9_tokio_telemetry::background_task::BackgroundTaskConfig;
use dial9_tokio_telemetry::background_task::s3::S3Config;

let writer = RotatingWriter::new("/tmp/traces/trace.bin", 1_MB, 5_MB)?;

let s3_config = S3Config::builder()
    .bucket("my-traces")
    .prefix("prod")
    .service_name("checkout-api")
    .instance_path("us-east-1/i-0abc123")
    .boot_id("unique-boot-id")
    .build();

let uploader_config = BackgroundTaskConfig::builder()
    .trace_path("/tmp/traces/trace.bin")
    .s3(s3_config)
    .build();

let (runtime, guard) = TracedRuntime::builder()
    .with_task_tracking(true)
    .with_s3_uploader(uploader_config)
    .build_and_start(builder, writer)?;

// Graceful shutdown: flush, seal, wait for worker to drain
guard.graceful_shutdown(Duration::from_secs(30)).await?;
```

`BackgroundTaskConfig` also accepts:
- `poll_interval` — how often to scan for sealed segments (default: 1s)
- `client` — pre-built `aws_sdk_s3::Client` (skips default SDK config loading)
- `metrics_sink` — `BoxEntrySink` for pipeline metrics (default: dev-null)

## Worker Loop

```rust
// WorkerLoop::run — runs on dedicated thread with its own current_thread runtime
loop {
    if stop.load(Acquire) {
        drain_remaining();  // process all sealed segments one last time
        return;
    }

    let sealed = find_sealed_segments(dir, stem)?;  // sorted oldest-first

    for segment in sealed {
        let bytes = fs::read(&segment.path)?;  // skip if NotFound (evicted)
        let mut data = SegmentData { segment, bytes, metadata, metrics };

        for processor in &mut pipeline {
            data = processor.process(data).await?;  // on error: log, skip segment
        }
    }

    sleep(poll_interval).await;
}
```

## Error Handling

| Error | Action | State Change |
|-------|--------|--------------|
| Segment disappeared (evicted by RotatingWriter) | Skip, log debug | None (circuit breaker unaffected) |
| S3 upload fails (500, timeout, 403) | Log warning, keep file | Circuit breaker → Open |
| S3 retry succeeds | Log info | Circuit breaker → Closed |
| Compression fails | Log error, skip segment | None |
| Circuit breaker open | Skip upload entirely | None (wait for backoff timer) |

**Never crash.** All errors are logged via `tracing`. Worker continues processing.

Per-segment metrics are recorded regardless of success or failure — the `SegmentData` carries a metrics guard that flushes on drop.

## Metrics

The worker emits per-segment metrics via `metrique`:

| Metric | Description |
|--------|-------------|
| `TotalTime` | End-to-end processing time (ms) |
| `Success` | 1 on success, 0 on failure |
| `SegmentIndex` | Segment index from RotatingWriter |
| `UncompressedSize` | Raw segment size (bytes) |
| `CompressedSize` | After gzip (bytes) |
| `Gzip.Time` | Compression time (ms) |
| `Gzip.Success` | Whether compression succeeded |
| `S3Upload.Time` | Upload time (ms) |
| `S3Upload.Success` | Whether upload succeeded |

Pipeline stage metrics are prefixed with the processor name automatically.

## S3 Object Layout

```
s3://{bucket}/{prefix}/{date-time}/{service}/{instance}/{epoch_secs}-{index}.bin.gz
```

- `{date-time}`: `2026-03-07/2030` — 1-minute bucket (enables time-range queries across all services)
- `{service}`: user-provided service name
- `{instance}`: `us-east-1/i-0abc123` or `dc-west/rack4-host7` (opaque string)
- `{epoch_secs}`: Unix epoch seconds (parsed from `SegmentMetadata` header, falls back to file mtime)
- `{index}`: segment index from RotatingWriter

Extension is `.bin.gz` when compressed, `.bin` when not.

**Metadata headers** (set via S3 SDK `.metadata()` — the SDK auto-adds the `x-amz-meta-` prefix):
```
service: checkout-api
boot-id: a3f7c2d1-...
segment-index: 3
start-time: 1741384542
host: i-0abc123
```

## Backpressure

If uploads fall behind, sealed files accumulate. `RotatingWriter` already handles this: when total disk usage exceeds `max_total_size`, it deletes the oldest files.

Worker processes oldest-first to maximize the window before eviction.

## Graceful Shutdown

```rust
impl TelemetryGuard {
    pub async fn graceful_shutdown(self, timeout: Duration) -> Result<(), std::io::Error> {
        // 1. Stop flush thread (AtomicBool signal)
        // 2. Seal final segment (.active → .bin)
        // 3. Signal worker to stop (separate AtomicBool)
        // 4. Worker drains remaining segments, then exits
        // 5. Wait for worker thread to join (with timeout)
    }
}
```

The worker checks its `AtomicBool` stop signal each loop iteration. When set: process all remaining sealed segments one final time, then exit. `Drop` on `TelemetryGuard` performs the same sequence synchronously (without a timeout).

## Testing Strategy

Use [`s3s`](https://docs.rs/s3s/) for integration tests. It implements the S3 wire protocol, so tests exercise the real AWS SDK against a local fake server. Both `aws_sdk_s3_transfer_manager::Client` (for uploads) and `aws_sdk_s3::Client` (for read-back verification) are wired to the same `s3s-fs` backend.

**Key tests:**
1. End-to-end: RotatingWriter seals → worker uploads to s3s → verify object contents and metadata
2. Compression roundtrip: upload gzipped → download from s3s → decompress → verify identical
3. S3 metadata headers: verify `service`, `boot-id`, `segment-index`, `start-time`, `host` via `HeadObject`
4. Eviction tolerance: missing segment file doesn't trip circuit breaker
5. Region auto-detection: `HeadBucket` with wrong region → corrects via response header
6. Stress test: high segment throughput → all segments uploaded and valid

## Future Work

- **Symbolization:** Embed `/proc/self/maps` in traces, symbolize in worker (as a pipeline processor)
- **Sidecar mode:** Run worker as separate process for blast-radius isolation
- **Cross-host indexing:** S3 event → Lambda → DynamoDB for "find all traces matching X"
- **Parquet output:** Convert traces to Parquet for Athena queries
