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
                        S3: {bucket}/{prefix}/{service}/
                            {date-hour}/{instance}/
                            {timestamp}-{index}.bin.gz
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

### 2. Time-bucketed S3 keys

**Problem:** Finding traces for a specific time range requires listing all objects.

**Solution:** Include date-hour in the key path:

```
{prefix}/{service}/{date-hour}/{instance}/{timestamp}-{index}.bin.gz
```

Example: `traces/checkout-api/2026-03-07/20/i-0abc123/2026-03-07T20-35-42Z-3.bin.gz`

**Benefits:**
- Time-range queries: `aws s3 ls s3://bucket/traces/my-service/2026-03-07/20/`
- Natural Athena partitioning if we add Parquet output later
- Efficient S3 lifecycle policies per time bucket

**Tradeoff:** Requires clock sync, but we already need that for trace timestamps.

### 3. Gzip compression

**Problem:** Trace files are large (binary event streams).

**Solution:** Gzip in memory before upload. Trace data is highly compressible (repetitive structures).

**Why gzip not zip?** Simpler, standard `Content-Encoding` header, better compression ratio.

### 4. Connection state machine

**Problem:** S3 outages shouldn't crash the worker or lose data.

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

**Solution:** Three tiers:

```toml
# Core only (no worker)
dial9-tokio-telemetry = "0.1"

# Worker without S3 (future: symbolization only)
dial9-tokio-telemetry = { version = "0.1", features = ["worker"] }

# Worker with S3
dial9-tokio-telemetry = { version = "0.1", features = ["worker-s3"] }
```

`worker-s3` pulls in `aws-sdk-s3`, `aws-sdk-s3-transfer-manager`, `aws-config`, `flate2`.

## API

```rust
// In-process worker with S3 upload
let writer = RotatingWriter::new("/tmp/traces/trace.bin", 1_MB, 5_MB)?;

let worker_config = WorkerConfig::builder()
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
    .in_process_worker(worker_config)
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
s3://{bucket}/{prefix}/{service}/{date-hour}/{instance}/{timestamp}-{index}.bin.gz
```

- `{date-hour}`: `2026-03-07/20` (enables time-range queries)
- `{instance}`: `us-east-1/i-0abc123` or `dc-west/rack4-host7` (opaque string)
- `{timestamp}`: `2026-03-07T20-35-42Z` (ISO 8601, colons → dashes)
- `{index}`: segment index from RotatingWriter

**Metadata headers:**
```
x-amz-meta-service: checkout-api
x-amz-meta-boot-id: a3f7c2d1-...
x-amz-meta-segment-index: 3
x-amz-meta-start-time: 2026-03-07T20:35:42Z
x-amz-meta-host: i-0abc123
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

- **Symbolization:** Embed `/proc/self/maps` in traces, symbolize in worker
- **Sidecar mode:** Run worker as separate process for blast-radius isolation
- **Cross-host indexing:** S3 event → Lambda → DynamoDB for "find all traces matching X"
- **Parquet output:** Convert traces to Parquet for Athena queries
