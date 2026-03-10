# S3 Worker Design

## Overview

Get trace data from running processes into S3 with minimal in-process overhead. The application writes traces to local disk; a background worker uploads them asynchronously.

**Core principle:** Keep the hot path simple. Push all heavy work (S3 uploads, compression, retries) to a worker that can't affect application performance.

## Architecture

```
Application Process              Worker Thread
┌─────────────────┐             ┌──────────────────────┐
│ TracedRuntime    │             │ 1. Watch for sealed   │
│   RotatingWriter │────────────▶│    segments           │
│   /tmp/traces/   │  .bin files │ 2. Gzip compress      │
│                  │             │ 3. Upload to S3       │
│                  │             │ 4. Delete local file  │
└─────────────────┘             └──────────────────────┘
                                           │
                                           ▼
                        S3: {bucket}/{prefix}/{date-time}/
                            {service}/{instance}/
                            {epoch_secs}-{index}.bin.gz
```

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

**Decision:** Time (10-minute bucket) is the first component after the optional prefix:

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
- 10-minute bucketing gives 144 prefixes per day — manageable for listing and lifecycle policies

**Tradeoff:** Requires reasonable clock sync, but we already need that for trace timestamps.

### 3. Gzip compression

**Problem:** Trace files are large (binary event streams).

**Solution:** Gzip in memory before upload. Trace data is highly compressible (repetitive structures).

**Why gzip not zip?** Simpler, standard `Content-Encoding` header, better compression ratio.

**Why gzip and not zstd?** Zstd has better compression ratios and speed, but gzip has wider ecosystem support: S3 `Content-Encoding: gzip` is universally understood, every CLI tool can decompress it (`gunzip`, `zcat`), and the `flate2` crate is already a transitive dependency via the AWS SDK. Switching to zstd would add a native C dependency (`zstd-sys`) for marginal gains on files that are typically 1-5 MB. If compression becomes a bottleneck, zstd is a straightforward swap.

### 4. Connection state machine

**Problem:** S3 outages shouldn't crash the worker or lose data.

**Disk space safety:** Running out of disk space is worse than losing trace data. `RotatingWriter` enforces a `max_total_size` budget — when total disk usage exceeds the limit, it deletes the oldest sealed segments. This means if S3 is unreachable and files accumulate, the writer evicts old segments to stay within bounds. Data loss is acceptable; disk exhaustion is not. The worker processes oldest-first to maximize the upload window before eviction.

**Solution:** Track connection health, degrade gracefully:

```
Healthy: upload + delete
   ↓ (upload fails)
Degraded: skip uploads, keep files on disk, exponential backoff
   ↓ (retry succeeds)
Healthy
```

Backoff: 1s → 2s → 4s → ... → 5min cap

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

`worker-s3` pulls in `aws-sdk-s3`, `aws-sdk-s3-transfer-manager`, `aws-config`, `flate2`.

## API

```rust
// In-process worker with S3 upload
let writer = RotatingWriter::new("/tmp/traces/trace.bin", 1_MB, 5_MB)?;

let uploader_config = UploaderConfig::builder()
    .trace_path("/tmp/traces/trace.bin")
    .s3(S3Config::builder()
        .bucket("my-traces")
        .prefix("prod")
        .service_name("checkout-api")
        .instance_path("us-east-1/i-0abc123")
        .boot_id("unique-boot-id")
        .build()?)
    .build()?;

let (runtime, guard) = TracedRuntime::builder()
    .with_s3_uploader(uploader_config)
    .build_and_start(builder, writer)?;

// Graceful shutdown: flush, seal, wait for worker to drain
guard.graceful_shutdown(Duration::from_secs(30)).await?;
```

## Worker Loop

```rust
loop {
    let sealed = find_sealed_segments()?;  // sorted oldest-first
    
    for segment in sealed {
        let data = fs::read(&segment.path)?;
        let compressed = gzip_compress(&data)?;
        let key = s3_config.object_key(&segment, &timestamp());
        
        if connection.should_attempt_upload() {
            match upload(&key, compressed).await {
                Ok(_) => {
                    connection.on_success();
                    fs::remove_file(&segment.path)?;
                }
                Err(e) => {
                    connection.on_failure();
                    tracing::warn!("upload failed: {e}");
                    // File stays on disk, retry next loop
                }
            }
        }
    }
    
    sleep(poll_interval).await;
}
```

## Error Handling

| Error | Action | State Change |
|-------|--------|--------------|
| Segment disappeared (evicted by RotatingWriter) | Skip, log debug | None |
| S3 upload fails (500, timeout, 403) | Log warning, keep file | → Degraded |
| S3 retry succeeds | Log info | → Healthy |
| Compression fails | Log error, skip segment | None |

**Never crash.** All errors are logged. Worker continues processing.

## S3 Object Layout

```
s3://{bucket}/{prefix}/{date-time}/{service}/{instance}/{epoch_secs}-{index}.bin.gz
```

- `{date-time}`: `2026-03-07/2030` — 10-minute bucket (enables time-range queries across all services)
- `{service}`: user-provided service name
- `{instance}`: `us-east-1/i-0abc123` or `dc-west/rack4-host7` (opaque string)
- `{epoch_secs}`: Unix epoch seconds
- `{index}`: segment index from RotatingWriter

**Metadata headers** (set via S3 SDK `.metadata()` — the SDK auto-adds the `x-amz-meta-` prefix):
```
service: checkout-api
boot-id: a3f7c2d1-...
segment-index: 3
start-time: 2026-03-07T20:35:42Z
host: i-0abc123
```

## Backpressure

If uploads fall behind, sealed files accumulate. `RotatingWriter` already handles this: when total disk usage exceeds `max_total_size`, it deletes the oldest files.

Worker processes oldest-first to maximize the window before eviction.

## Graceful Shutdown

```rust
impl TelemetryGuard {
    pub async fn graceful_shutdown(self, timeout: Duration) -> Result<()> {
        // 1. Disable recording, flush, seal final segment
        // 2. Write .shutdown sentinel
        // 3. Wait for worker to drain (with timeout)
        // 4. Kill worker if timeout expires
    }
}
```

Worker checks for `.shutdown` sentinel each loop. When found: process remaining segments, then exit.

## Testing Strategy

Use [`s3s`](https://docs.rs/s3s/) for integration tests. It implements the S3 wire protocol, so tests exercise the real AWS SDK against a local fake server.

**Key tests:**
1. End-to-end: RotatingWriter seals → worker uploads to s3s → verify object
2. Fault injection: s3s returns 500s → worker enters degraded → retries → recovers
3. Backpressure: slow s3s → RotatingWriter evicts old segments → worker skips gracefully
4. Compression: upload → download from s3s → decompress → verify roundtrip

## Future Work

- **Metrics:** The worker should emit metrics for upload success/failure counts, upload latency, compression ratios, and circuit breaker state. Eventually these should also be recorded into the dial9 traces themselves for self-monitoring.
- **Symbolization:** Embed `/proc/self/maps` in traces, symbolize in worker
- **Sidecar mode:** Run worker as separate process for blast-radius isolation
- **Cross-host indexing:** S3 event → Lambda → DynamoDB for "find all traces matching X"
- **Parquet output:** Convert traces to Parquet for Athena queries
