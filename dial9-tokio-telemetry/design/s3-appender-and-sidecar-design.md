# S3 Appender & Sidecar Architecture — Design

## Implementation workstreams

Three independent workstreams, ordered by dependency. Workstream A and B can
proceed in parallel. Workstream C depends on both.

### Workstream A: Sealed file pipeline + S3 upload

Core path: RotatingWriter seals files → worker watches → uploads to S3.
No symbolization yet — just get raw traces into S3.

- [ ] A1. Remove `SimpleBinaryWriter` (replace all usages with `RotatingWriter`)
- [ ] A2. `RotatingWriter` rename-on-seal (`.active` → `.bin`)
- [ ] A3. `SegmentMetadata` event (written at start of each segment)
- [ ] A4. Sealed-file watcher (finds `.bin` files, ignores `.active`, oldest-first)
- [ ] A5. S3 uploader with `aws-sdk-s3-transfer-manager` (gzip + upload + delete)
- [ ] A6. S3 object metadata headers on `PutObject`
- [ ] A7. Machine identity detection (`ImdsMetadata` or explicit string)
- [ ] A8. S3 connection state machine (healthy/degraded, backoff, retry)
- [ ] A9. `WorkerConfig` / `S3Config` builders, `worker` / `worker-s3` feature flags
- [ ] A10. In-process worker integration (`.in_process_worker()` on builder)
- [ ] A11. Graceful shutdown (`.graceful_shutdown().await`)

### Workstream B: Symbolization via process maps

Move symbolization out of the flush thread. Traces become self-contained.

- [ ] B1. `ProcessMaps` event type (format change, version bump)
- [ ] B2. Emit `ProcessMaps` as last event before seal (in `RotatingWriter::rotate`)
- [ ] B3. Emit `ProcessMaps` on final flush (`TelemetryGuard::drop`)
- [ ] B4. `TraceReader` parses `ProcessMaps` events
- [ ] B5. Unknown event tag handling (verify forward compat)
- [ ] B6. Symbolizer: read `ProcessMaps` + `CpuSample` → resolve addresses
- [ ] B7. Symbol table footer: append to sealed segment, footer length at EOF
- [ ] B8. Remove old in-process symbolization from flush thread

### Workstream C: Sidecar process (optimization)

Move the worker out-of-process for blast-radius isolation. Depends on A + B.

- [ ] C1. `entry_point!()` macro (argv interception, proof token)
- [ ] C2. Sidecar spawn with `PR_SET_PDEATHSIG(SIGKILL)` via `pre_exec`
- [ ] C3. Re-entrant `credential_process` (`--dial9-credentials` flag)
- [ ] C4. Sidecar logging → app `tracing` (stderr pipe forwarding)
- [ ] C5. `.worker(Dial9Worker)` API on builder
- [ ] C6. Crash detection (parent PID monitoring, `.active` file handling)

---

## Overview

This document describes the design for getting trace data from a running process
into S3, with out-of-process symbolization. The core principle is **keep the
in-process path as simple as possible** and push all heavy lifting (symbolization,
S3 uploads, indexing) to a worker that can run either in-process or as a
separate sidecar process.

### Goals

- Zero new dependencies or network calls in the application process's hot path
- Symbolization moves out of the flush thread into the worker
- Trace data lands in S3 for cross-host discovery and long-term storage
- Self-contained trace files: a `.bin` downloaded from S3 can be symbolized
  without external context

### Non-goals (for now)

- Cross-host query/indexing is a separate concern (see §7 for a sketch)
- The sidecar's internal state machine is out of scope for this doc

### Development approach

All work should follow red/green TDD:

1. **Red:** Write a failing test that captures the desired behavior
2. **Green:** Write the minimal code to make the test pass
3. **Refactor:** Clean up while keeping tests green

Use `s3s` (see §10) for S3 integration tests so we can test at the wire level
from the start. Each section below notes the key test cases to write first.

---

## 1. Architecture

```
┌──────────────────────────┐       ┌─────────────────────────────────┐
│     Application Process   │       │     Worker (thread or sidecar)   │
│                           │       │                                   │
│  TracedRuntime             │       │  1. Detect sealed segments        │
│    └─ RotatingWriter       │──────►│  2. Symbolize CPU samples         │
│       /tmp/traces/{boot}/  │ local │  3. Upload to S3                  │
│                           │ files │  4. Delete local file             │
│                           │       │                                   │
└──────────────────────────┘       └───────────────┬───────────────────┘
                                                    │
                                                    ▼
                                     S3: {bucket}/{prefix}/{region}/
                                         {host}/{boot_id}/segment-{N}.bin
                                                    │
                                                    ▼ (S3 PutObject event)
                                          Lambda → DynamoDB index
```

The application process uses `RotatingWriter` exactly as it does today. No
`TraceWriter` trait changes are needed for the core path. The worker runs the
same pipeline code whether spawned as a background thread (in-process) or as a
separate OS process (sidecar via `entry_point!()`).

---

## 2. Unified worker model

The worker is a pipeline that processes sealed trace segments. The same code
runs either in-process (background thread) or out-of-process (sidecar). The
only difference is who spawns it.

### 2.1 Pipeline

```
watch for sealed segments → symbolize → upload to S3 → delete local
```

Both symbolization and S3 upload are optional and independently configurable.

### 2.2 Execution modes

| Mode | Symbolization | S3 Upload | AWS SDK dep | `entry_point!()` required |
|---|---|---|---|---|
| No worker (today) | ❌ | ❌ | No | No |
| In-process worker | ✅ background thread | ✅ (optional) | Yes | No |
| Sidecar worker | ✅ separate process | ✅ (optional) | Yes (in sidecar) | Yes |

### 2.3 API

All configuration uses builders (`#[bon::builder]` v3) to maintain backwards
compatibility. No positional config arguments.

```rust
// Sidecar mode — entry_point!() returns a proof token
fn main() {
    let worker = dial9::entry_point!();
    TracedRuntime::builder()
        .worker(worker.with_s3(
            S3RemoteConfig::builder()
                .bucket("my-bucket")
                .prefix("my-prefix")
                .build()))
        .build_and_start(builder, Box::new(writer))?;
}

// In-process mode — can pass SdkConfig directly (no process boundary)
fn main() {
    TracedRuntime::builder()
        .in_process_worker(
            WorkerConfig::builder()
                .symbolize(true)
                .s3(S3Config::builder()
                    .bucket("my-bucket")
                    .prefix("my-prefix")
                    .sdk_config(sdk_config)
                    .build())
                .build())
        .build_and_start(builder, Box::new(writer))?;
}

// No worker — local traces only, as today
fn main() {
    TracedRuntime::builder()
        .build_and_start(builder, Box::new(writer))?;
}
```

`Dial9Worker` is an opaque token returned by `entry_point!()` that cannot be
constructed any other way. It proves the macro was called (and the
`--dial9-credentials` interception is wired up). `.worker()` requires this
token; `.in_process_worker()` does not.

Note: `.worker()` takes `S3RemoteConfig` (only serializable fields — bucket,
prefix, profile, region). `.in_process_worker()` takes `S3Config` which can
additionally accept an `SdkConfig` directly. This type-level split prevents
accidentally passing a non-serializable `SdkConfig` across the process boundary.

### 2.4 Module structure

The worker lives in the main crate behind feature flags to keep the dependency
footprint minimal for users who don't need it:

```
dial9-tokio-telemetry/
  src/
    telemetry/
      ...                    # existing modules
    worker/
      mod.rs               # TracePipeline, WorkerConfig
      symbolizer.rs        # Symbolizer
      s3.rs                # S3Uploader, S3Config, S3RemoteConfig
      sealed.rs            # sealed-file detection
    lib.rs
```

```toml
[features]
worker = []                              # symbolization only
worker-s3 = ["worker", "dep:aws-sdk-s3", "dep:aws-sdk-s3-transfer-manager", "dep:aws-config"]
```

- No feature flags → core only (`TracedRuntime`, `RotatingWriter`, format)
- `worker` → adds symbolization pipeline, no AWS deps
- `worker-s3` → adds S3 upload support

### 2.5 TDD milestones

1. `TracePipeline` finds sealed `.bin` files and ignores `.active` files
2. `TracePipeline` reads and re-writes a sealed segment (identity transform)
3. `TracePipeline` symbolizes CPU samples using embedded `ProcessMaps`
4. `TracePipeline` uploads symbolized segment to S3 (via `s3s` fake)
5. `TracePipeline` deletes local file after confirmed upload
6. End-to-end: RotatingWriter seals → pipeline symbolizes → uploads → deletes

---

## 3. In-process changes

### 3.1 Process maps in the trace

CPU sample addresses are virtual addresses that require `/proc/self/maps` to
symbolize. Today, symbolization happens in-process so it can read maps directly.
With the worker model, the maps must be embedded in the trace.

**Design:** Emit a `ProcessMaps` event as the **last event before sealing** a
segment (i.e., during rotation and on final shutdown flush).

```rust
TelemetryEvent::ProcessMaps {
    entries: Vec<MapEntry>,
}

struct MapEntry {
    start: u64,
    end: u64,
    offset: u64,
    path: String,  // e.g. "/usr/bin/my-service", "[vdso]"
}
```

**Why at the end, not the beginning?** `dlclose` is extremely rare in practice.
Libraries are loaded at startup or lazily via `dlopen` and never unloaded. The
maps at segment end are a superset of everything mapped during the segment's
lifetime, so all CPU sample addresses in the segment will be covered.

**Crash case:** The last segment won't have maps. This is acceptable — crash
traces are best-effort. The sidecar can attempt to read `/proc/{pid}/maps` if
the process is still alive as a fallback.

#### TDD milestones

1. `RotatingWriter` emits `ProcessMaps` as last event before rotation
2. `ProcessMaps` contains correct entries matching `/proc/self/maps`
3. `TelemetryGuard::drop` emits `ProcessMaps` before final flush
4. `TraceReader` can parse segments containing `ProcessMaps` events

### 3.2 Segment metadata

Optionally, a `SegmentMetadata` event can be written at the start of each
segment with key-value pairs (host, region, service, boot_id). This makes trace
files self-describing regardless of where they're stored.

```rust
TelemetryEvent::SegmentMetadata {
    entries: Vec<(String, String)>,
}
```

This is a nice-to-have and can be added independently.

### 3.3 RotatingWriter: rename-on-seal

To signal that a segment is complete, `RotatingWriter` writes to a `.active`
suffix and renames on rotation:

```
While writing:  trace.3.bin.active
After rotation: trace.3.bin          (atomic rename)
```

The change to `RotatingWriter::rotate()` is small:

```rust
// On creation of a new segment:
let active_path = Self::file_path(&self.base_path, self.next_index)
    .with_extension("bin.active");
let file = File::create(&active_path)?;

// On sealing the previous segment (in rotate()):
fs::rename(&old_active_path, &sealed_path)?;  // atomic on Linux
```

On clean shutdown, `TelemetryGuard::drop` writes the final `ProcessMaps` event,
flushes, and renames the last segment.

#### TDD milestones

1. New segments are created with `.active` suffix
2. Rotation renames `.active` → `.bin`
3. Clean shutdown renames the final `.active` file
4. Existing `RotatingWriter` tests still pass (backwards compatible behavior)

---

## 4. Sealed file detection

The worker needs to know when a segment is safe to read. Two mechanisms,
used together:

### 4.1 Rename signal (primary)

Files without the `.active` suffix are sealed. The worker only processes
`trace.{N}.bin` files, never `.active` files. This is unambiguous and atomic.

### 4.2 Crash detection (fallback)

If the application crashes, the last `.active` file is never renamed. The
sidecar detects this by checking if the parent process is still alive
(`kill(pid, 0)` or `/proc/{pid}`). The app PID is passed to the sidecar at
spawn time. If the process is dead and an `.active` file exists, the sidecar
treats it as sealed (without maps — best-effort).

### 4.3 Alternative: socket-based signaling

Instead of filesystem-based detection, the app and sidecar could communicate
over a Unix domain socket. The app sends a message on each rotation:
`{"sealed": "trace.3.bin"}`. This is more immediate and avoids filesystem
polling, but adds complexity to both sides.

**Decision:** The communication channel between the app and sidecar (filesystem
vs socket vs shared memory) is a key design decision for the sidecar's state
machine, which warrants its own focused discussion. The rename-on-seal approach
is a solid starting point that works without any IPC, but a socket-based
protocol may be preferable once the sidecar's full responsibilities are scoped.

#### TDD milestones

1. Worker finds sealed `.bin` files and ignores `.active` files
2. Worker processes segments oldest-first (by index)
3. Worker detects dead parent PID and treats `.active` as sealed
4. Worker skips `.active` files when parent is still alive

---

## 5. S3 object layout

```
s3://{bucket}/{prefix}/{service_name}/{instance_path}/{timestamp}-{index}.bin.gz
```

Where:
- `{service_name}` is user-provided (required) — identifies the logical
  workload. Allows multiple services to share one bucket.
- `{instance_path}` is a pluggable metadata string that identifies where the
  process is running. Can contain `/` for hierarchical grouping. Provided
  either as a literal string or via a metadata provider:

  ```rust
  S3Config::builder()
      .bucket("my-traces")
      .service_name("checkout-api")
      // Explicit string:
      .instance_path("us-east-1/i-0abc123")
      // Or auto-detect (future):
      .instance_path(ImdsMetadata::detect().await)
      // Or on-prem / non-AWS:
      .instance_path("dc-west/rack4-host7")
      .build()
  ```

- `{timestamp}` is ISO 8601 with colons replaced by dashes (e.g.
  `2026-03-05T19-30-00Z`)
- `{index}` is the segment index from `RotatingWriter`

The library does not interpret `{instance_path}` — it is an opaque path
segment. This keeps the design portable across cloud providers and on-prem.

### 5.1 Metadata providers

`ImdsMetadata` (future work, separate task) auto-detects the environment and
returns an appropriate path string:

| Environment | `instance_path` |
|---|---|
| EC2 | `{region}/{instance_id}` |
| ECS/Fargate | `{region}/task-{task_id}` |
| Lambda | `{region}/lambda-{env_id}` |
| Fallback | `unknown/{hostname}` |

Can reuse or take inspiration from the
[async-profiler metadata module](https://github.com/async-profiler/rust-agent/blob/main/src/metadata/mod.rs#L42)
which already handles EC2, ECS/Fargate, and fallback cases.

### 5.2 Drill-down access pattern

For AWS with `ImdsMetadata`, the hierarchy maps to a natural UI drill-down:
service → region → instance → segments.

```
ListObjectsV2(Prefix = "traces/checkout-api/")                          → all regions
ListObjectsV2(Prefix = "traces/checkout-api/us-east-1/")                → instances in region
ListObjectsV2(Prefix = "traces/checkout-api/us-east-1/i-0abc123/")      → one instance
```

### 5.3 Compression and format

Objects are gzip-compressed (not zip). The trace format is binary and already
requires our tooling to read, so there's no benefit to a human-readable
container. The Lambda indexer will have the reader linked in regardless.

`boot_id` is not in the key path — it lives inside the `SegmentMetadata` event
for programmatic correlation of segments from the same run.

### 5.4 S3 object metadata

Each `PutObject` also sets `x-amz-meta-*` headers with key metadata:

```
x-amz-meta-service: checkout-api
x-amz-meta-boot-id: a3f7c2d1-...
x-amz-meta-segment-index: 3
x-amz-meta-start-time: 2026-03-05T19:30:00Z
x-amz-meta-host: i-0abc123
```

This is not for discovery (S3 can't filter on metadata). It's for quick
inspection of a specific object via `HeadObject` without downloading the full
segment. Useful for:

- UI showing segment details before download
- Lambda indexer — can index from `HeadObject` alone without parsing the
  binary trace format
- CLI debugging: `aws s3api head-object --bucket ... --key ...`

The authoritative metadata remains the `SegmentMetadata` event inside the
trace file (works offline, self-contained). The S3 headers are a convenience
copy.

### 5.1 Why not S3 object metadata?

S3 user-defined metadata (`x-amz-meta-*`) cannot be filtered on via
`ListObjectsV2` and requires a per-object `HeadObject` call to read. It's
not useful for discovery at scale.

### 5.2 Why not a metadata sidecar file?

A per-segment `.meta.json` doubles the number of S3 PUTs and can get out of
sync. Embedding metadata in the trace file itself (§3.2) is more robust.

---

## 6. S3 upload & credentials

The worker runs its own tokio runtime for async S3 operations.

- **Client:** `aws-sdk-s3` with standard credential chain
- **Retries:** SDK default retry with backoff
- **Concurrency:** bounded (e.g., 4 concurrent uploads)
- **Confirmation:** delete local file only after `PutObject` succeeds

### 6.1 Credential resolution

**In-process worker:** The application constructs an `SdkConfig` (with whatever
credential provider it needs) and passes it directly via `S3Config::builder()
.sdk_config(config)`. No serialization boundary, so any credential provider
works.

**Sidecar worker:** Cannot receive an `SdkConfig` across the process boundary.
Uses the standard AWS credential chain (`aws_config::load_defaults`), which
covers the common cases automatically:

- EC2 instance profiles (IMDS)
- ECS task roles (`AWS_CONTAINER_CREDENTIALS_RELATIVE_URI`, inherited)
- Environment variables (`AWS_ACCESS_KEY_ID`, etc.)
- `~/.aws/config` profiles (SSO, AssumeRole chains)

For non-standard setups, the sidecar config supports an optional `profile`
override, which can point to any profile in `~/.aws/config` — including one
that uses `credential_process`.

### 6.2 Re-entrant credential provider

For exotic credential setups, the application binary can act as its own
`credential_process`. `entry_point!()` wires this up automatically:

```rust
fn main() {
    // entry_point!() intercepts --dial9-credentials:
    //   if present, emits credentials as JSON and exits.
    //   otherwise, returns a Dial9Worker handle.
    let dial9_worker = dial9::entry_point!();

    let (runtime, _guard) = TracedRuntime::builder()
        .worker(dial9_worker.with_s3(
            S3RemoteConfig::builder()
                .bucket("my-bucket")
                .build()))
        .build_and_start(builder, Box::new(writer))?;
}
```

When `entry_point!()` spawns the sidecar, it generates a config with:

```ini
[profile dial9-traces]
credential_process = /path/to/current/exe --dial9-credentials
```

The sidecar uses this profile, which calls back into the application binary to
resolve credentials. This means:

- The app's existing credential logic (AssumeRole, Vault, etc.) is reused
  as-is — no duplication.
- The sidecar remains generic — it just uses the standard AWS SDK profile
  mechanism.
- `entry_point!()` knows `std::env::current_exe()` and wires everything up
  automatically. No manual config needed.
- The credential subprocess is short-lived (resolve + print + exit), so it
  doesn't carry the overhead of the full application.

#### TDD milestones

1. Worker uploads a sealed segment to fake S3 (`s3s`) and verifies object key
2. Worker retries on transient S3 failure (500) — `s3s` fault injection
3. Worker deletes local file only after confirmed upload
4. Worker does not delete local file on upload failure
5. Re-entrant credential provider: `--dial9-credentials` flag emits valid JSON
6. Sidecar resolves credentials via `credential_process` profile

---

## 7. Cross-host discovery (future work)

S3 prefix listing works for single-host queries but doesn't scale for
"find all traces matching X across all hosts." A lightweight indexing layer
can be added independently:

**S3 Event → Lambda → DynamoDB:**

1. S3 `PutObject` event triggers a Lambda
2. Lambda parses metadata from the key path
3. Lambda writes to a DynamoDB table with GSIs for common query patterns
   (region + time range, host, service)

This is fully decoupled from the writer — traces are safe in S3 regardless of
whether the indexer is deployed. The indexer can be retroactively run against
existing objects.

---

## 8. Backpressure

If S3 uploads fall behind, sealed files accumulate on disk. The
`RotatingWriter` in the app process already handles this — it deletes the
oldest files when total disk usage exceeds `max_total_size`. So if the worker
can't keep up, the oldest unsymbolized segments are dropped. This is the right
tradeoff: telemetry should never exhaust disk space.

The worker should process segments oldest-first to maximize the window before
the app's rotation evicts them.

#### TDD milestones

1. Worker processes segments oldest-first (lowest index first)
2. Worker handles missing segments gracefully (already evicted by RotatingWriter)
3. Under slow S3 (`s3s` with injected latency), RotatingWriter evicts old
   segments and worker skips them without error

---

## 9. Worker state machine

The worker must never crash, never exit unexpectedly, and never affect the
application. All errors are log lines forwarded to the app's `tracing` logger.

### 9.1 Segment states

```
┌──────────┐
│  Active   │  (.bin.active — app is writing, worker ignores)
└────┬─────┘
     │ app rotates (rename)
     ▼
┌──────────┐
│  Sealed   │  (.bin — ready for processing)
└────┬─────┘
     │ worker picks it up
     ▼
┌──────────────┐
│ Symbolizing   │  best-effort: unresolved frames stay raw
└────┬─────────┘
     │
     ▼
┌──────────────┐
│  Uploading    │  aws-sdk-s3-transfer-manager
└────┬─────────┘
     │ confirmed
     ▼
┌──────────┐
│  Deleted  │  local file removed
└──────────┘
```

Processing is strictly sequential — one segment at a time, oldest first. No
pipelining in v1. If throughput becomes a problem, concurrency can be added
later.

### 9.2 S3 connection state

The worker tracks S3 connectivity as a separate state:

```
                  ┌─────────────────┐
       startup    │   Validating     │  HeadBucket
                  └───┬─────────┬───┘
                      │         │
                 success      failure
                      │         │
                      ▼         ▼
              ┌──────────┐  ┌──────────────┐
              │  Healthy  │  │   Degraded    │
              └─────┬────┘  └──────┬───────┘
                    │              │
              upload fails    periodic retry succeeds
                    │              │
                    ▼              ▼
              ┌──────────────┐  ┌──────────┐
              │   Degraded    │  │  Healthy  │
              └──────────────┘  └──────────┘
```

**Healthy:** symbolize + upload + delete. Normal operation.

**Degraded:** symbolize only, leave enriched files on disk. Retry S3
connectivity with exponential backoff (capped at e.g. 5 minutes). Log a
warning on each retry. When connectivity recovers, catch up on any segments
that haven't been evicted by `RotatingWriter`.

The worker never exits due to S3 failures. In degraded mode, symbolized files
on disk are still valuable — they can be manually retrieved or uploaded later.

### 9.3 Main loop

```rust
fn run(&mut self) {
    self.validate_s3();  // sets initial s3_state

    loop {
        let sealed = self.find_sealed_segments();  // sorted oldest-first

        if sealed.is_empty() {
            if self.parent_is_dead() {
                self.process_remaining_active_file();  // best-effort
                return;  // clean exit
            }
            sleep(self.poll_interval);
            self.maybe_retry_s3();  // periodic reconnect if degraded
            continue;
        }

        for segment in sealed {
            // Symbolize (always, regardless of S3 state)
            if let Err(e) = self.symbolize(&segment) {
                tracing::warn!("symbolization failed for {}: {e}", segment.display());
                // Continue anyway — upload raw segment
            }

            // Upload (only if S3 is healthy)
            if self.s3_is_healthy() {
                match self.upload(&segment) {
                    Ok(()) => {
                        fs::remove_file(&segment).ok();
                    }
                    Err(e) => {
                        tracing::warn!("S3 upload failed: {e}");
                        self.enter_degraded();
                        break;  // stop processing, will retry next loop
                    }
                }
            }
            // If degraded: symbolized file stays on disk
        }
    }
}
```

### 9.4 Error handling policy

| Error | Action | S3 state |
|---|---|---|
| Symbolization fails (missing ELF, corrupt) | Log warning, upload raw | Unchanged |
| Segment disappeared (evicted by RotatingWriter) | Log debug, skip | Unchanged |
| S3 upload transient failure (500, timeout) | Log warning, backoff | → Degraded |
| S3 upload permanent failure (403, NoSuchBucket) | Log error, backoff | → Degraded |
| S3 retry succeeds | Log info | → Healthy |
| Credential refresh failure | Log warning | → Degraded |
| Any panic in worker | Caught, logged, worker continues | Unchanged |

No error is ever propagated to the application. The worker catches panics at
the top level of its loop.

### 9.5 Logging

**In-process worker:** Logs directly via `tracing`, prefixed with
`target = "dial9_worker"`.

**Sidecar worker:** Spawned with `Stdio::piped()`. The `TelemetryGuard` runs
a small reader thread that reads the sidecar's stderr line-by-line and forwards
each line to the app's `tracing` logger at the appropriate level, with
`target = "dial9_worker"`.

```rust
// In the sidecar process:
tracing::warn!("S3 upload failed, entering degraded mode");

// Appears in the app's logs as:
// WARN dial9_worker: S3 upload failed, entering degraded mode
```

### 9.6 Process lifecycle — never leak the worker

The sidecar must be killed when the app exits, regardless of how it exits.

**Primary mechanism: `PR_SET_PDEATHSIG` at spawn time.**

```rust
unsafe {
    Command::new(current_exe)
        .args(&["--dial9-worker", ...])
        .pre_exec(|| {
            libc::prctl(libc::PR_SET_PDEATHSIG, libc::SIGKILL);
            Ok(())
        })
        .spawn()?;
}
```

`pre_exec` runs in the child after `fork` but before `exec` — no race window.
`SIGKILL` (not `SIGTERM`) ensures the worker cannot ignore the signal. This is
safe because the worker's state is idempotent (re-uploading a segment on
restart is fine).

This handles every exit scenario: normal shutdown, panic, `kill -9`, OOM
killer, `std::process::exit()`.

**Secondary mechanism: `TelemetryGuard::drop`** sends a graceful shutdown
signal (via `.shutdown` sentinel or SIGTERM) and waits briefly for the worker
to drain remaining segments. This is the polite path for normal shutdown;
`PR_SET_PDEATHSIG` is the safety net.

| Scenario | What kills the worker |
|---|---|
| Normal shutdown / `graceful_shutdown()` | `TelemetryGuard::drop` (graceful) |
| App panic (unwind) | `TelemetryGuard::drop` (graceful) |
| `kill -9`, OOM, `process::exit()` | `PR_SET_PDEATHSIG` → `SIGKILL` |

### 9.7 Graceful shutdown

```rust
impl TelemetryGuard {
    /// Flush remaining events, seal the final segment, and wait for the
    /// worker to finish processing. Returns when all segments are uploaded
    /// or the timeout expires.
    pub async fn graceful_shutdown(self, timeout: Duration) -> ShutdownResult {
        // 1. Disable recording, final flush, seal last segment
        // 2. Write .shutdown sentinel to trace dir
        // 3. Wait for worker to finish (with timeout)
        // 4. Return which segments were uploaded vs dropped
    }
}
```

The worker checks for the `.shutdown` sentinel each loop iteration. When found:
- Process all remaining sealed segments (symbolize + upload)
- Then exit cleanly

If the timeout expires, the guard kills the worker. Any unprocessed segments
remain on disk (symbolized or raw) — not lost, just not in S3.

Dropping the guard without calling `graceful_shutdown()` is also fine — the
last segment may be lost, which is acceptable.

### 9.8 Upload client

Use [`aws-sdk-s3-transfer-manager`](https://docs.rs/aws-sdk-s3-transfer-manager/latest/aws_sdk_s3_transfer_manager/)
for uploads. It handles multipart uploads for larger segments and manages
concurrency internally. The worker calls `.upload()` and the transfer manager
handles the rest.

### 9.9 Compression

Segments are gzip-compressed in memory before upload. Trace data is highly
compressible (repetitive binary event structures), so this significantly
reduces S3 storage costs and upload time.

```rust
fn compress_segment(data: &[u8]) -> io::Result<Vec<u8>> {
    let mut encoder = flate2::write::GzEncoder::new(
        Vec::new(),
        flate2::Compression::fast(),  // speed over ratio — this is on the hot path
    );
    encoder.write_all(data)?;
    encoder.finish()
}
```

The upload sets `Content-Encoding: gzip` so that S3 records the encoding.
Objects in S3 have the `.bin.gz` extension. Readers that download from S3
decompress transparently.

Reference: the [async-profiler rust-agent](https://github.com/async-profiler/rust-agent/blob/main/src/reporter/s3.rs)
uses a similar pattern (zip in memory → `PutObject`), with `spawn_blocking`
for the compression work.

### 9.10 S3 key naming

See §5 for the key layout. Machine identity is auto-detected at worker startup
(IMDS for EC2, ECS metadata endpoint for Fargate, `gethostname()` as fallback).

#### TDD milestones

1. Worker in healthy state: symbolize → upload → delete (s3s)
2. Worker enters degraded on S3 failure, symbolizes but doesn't delete
3. Worker recovers from degraded when S3 becomes available
4. Worker catches panics in symbolization and continues
5. Worker processes remaining segments after parent dies
6. Worker respects `.shutdown` sentinel and drains remaining segments
7. Worker logs forwarded from sidecar stderr to app's tracing
8. Backoff increases exponentially in degraded mode (capped)

---

## 10. Format changes

### 9.1 Segment layout

```
┌──────────────────────┐
│ Header (magic + ver) │
├──────────────────────┤
│ Events...            │  ← written by RotatingWriter in app process
│ ProcessMaps (last)   │
├──────────────────────┤
│ Symbol Table Footer  │  ← appended by worker after symbolization
│ Footer Length (u32)  │
└──────────────────────┘
```

The app process writes events and a `ProcessMaps` event as the final event
before sealing. The worker then reads the sealed file, resolves symbols from
the CPU sample addresses + process maps, and appends a symbol table as a
footer.

The `Footer Length` field at the very end of the file allows readers to seek
backwards to locate the symbol table. Readers that don't understand the footer
(older versions) simply stop when they encounter an unknown event tag — the
footer comes after all events, so forward compatibility is preserved.

### 9.2 New event variants

| Event | Written by | When | Purpose |
|---|---|---|---|
| `ProcessMaps { entries }` | App (RotatingWriter) | Last event before seal | Maps for symbolization |
| `SegmentMetadata { entries }` | App (RotatingWriter) | First event in segment | Self-describing traces (optional) |

### 9.3 Symbol table footer

The footer contains a mapping from instruction addresses to symbol names.
Its exact encoding is TBD, but conceptually:

```rust
struct SymbolTable {
    entries: Vec<SymbolEntry>,
}

struct SymbolEntry {
    address: u64,
    name: String,
    file: Option<String>,
    line: Option<u32>,
}
```

The worker reads `ProcessMaps` + `CpuSample` events from the segment, resolves
addresses against the mapped ELF binaries, and writes the symbol table as the
footer. The segment in S3 is fully self-contained: events, maps, and symbols.

#### TDD milestones

1. `write_header` + `write_event` + `ProcessMaps` produces a valid segment
2. `TraceReader` parses `ProcessMaps` and `SegmentMetadata` events
3. `TraceReader` skips unknown event tags gracefully (forward compat)
4. Symbol table footer can be read by seeking from end of file
5. Old `TraceReader` (without footer support) reads a segment with a footer
   without error (stops at unknown tag)

---

## 11. Testing strategy

### 10.1 Red/green TDD

All features are developed test-first:

1. Write a failing test that captures the desired behavior
2. Write the minimal code to make the test pass
3. Refactor while keeping tests green

Each section above includes specific TDD milestones. These should be
implemented in order — each milestone is a red→green cycle.

### 10.2 Fake S3 with s3s

Use [s3s](https://docs.rs/s3s/0.13.0/s3s/) to run a fake S3 server in tests.
`s3s` implements the S3 wire protocol, so tests exercise the real AWS SDK
client against a local server — no mocking at the Rust API level.

This enables:

- **End-to-end pipeline tests:** RotatingWriter seals a segment → worker
  picks it up → symbolizes → uploads to fake S3 → verify the object contents.
- **Fault injection:** `s3s` handlers can return 500s, throttle, drop
  connections, or inject latency to verify the worker retries correctly and
  doesn't lose data.
- **Backpressure testing:** slow down the fake S3 to simulate upload lag and
  verify the worker + RotatingWriter eviction interact correctly (oldest
  segments dropped, no unbounded memory growth).
- **Credential failure:** return 403s to verify the worker handles credential
  expiry / refresh gracefully.

### 10.3 Test layering

| Layer | What | How |
|---|---|---|
| Unit | Format read/write, sealed-file detection | In-memory, tempdir |
| Integration | Worker pipeline end-to-end | `s3s` fake S3, tempdir |
| Fault | Retry, backpressure, credential errors | `s3s` with injected faults |
| Sidecar | Process spawn, IPC, credential_process | Fork real binary in test |

---

## 12. Cleanup

### 12.1 Remove `SimpleBinaryWriter`

`SimpleBinaryWriter` is redundant — `RotatingWriter` with `max_file_size =
u64::MAX` and `max_total_size = u64::MAX` provides identical behavior (single
file, no rotation, no eviction). Remove `SimpleBinaryWriter` and update all
examples, tests, benches, and docs to use `RotatingWriter` instead.

This reduces the API surface and ensures all writers go through the
rename-on-seal path, which the worker depends on.

Affected files (35 usages across 15 files):
- `src/telemetry/writer.rs` — remove struct + impl
- `src/telemetry/mod.rs` — remove re-export
- Examples: `telemetry_demo`, `simple_workload`, `long_workload`,
  `blocking_sleep`, `cpu_profile_workload`, `realistic_workload`
- Tests: `echo_server`, `end_to_end`, `js_parser`
- Benches: `overhead_bench`
- Docs: `README.md`, `tokio-telemetry-system.md`

---

## 13. Open questions

1. **App ↔ sidecar communication channel:** Filesystem rename vs Unix socket
   vs shared memory. Affects latency of sealed-file detection, crash handling,
   and the sidecar's state machine design. Warrants a separate design
   discussion.

2. **Sidecar lifecycle:** What happens if the sidecar crashes? Should the app
   restart it? Should there be a watchdog? How does graceful shutdown
   coordinate between the two processes?

3. **Unknown event tag handling:** Does the current format reader skip unknown
   event types, or does it error? If it errors, the version bump needs
   careful rollout.

4. **Segment size tuning:** Larger segments = fewer S3 PUTs but more data at
   risk if the sidecar falls behind. Smaller segments = more PUTs but faster
   time-to-S3. Default of 5 MiB seems reasonable.

5. **Local-only mode:** Users who don't want S3 should still benefit from
   the worker for symbolization. The worker should work without S3 config
   (symbolize in place, leave files on disk).

6. **Symbol table footer encoding:** Exact binary format TBD. Needs to be
   compact (traces can have thousands of unique addresses) and fast to parse.
