# background_task module — Review & Analysis

## Summary

Reviewed the `background_task` module against the design doc (`design/s3-worker-design.md`) and the rest of the codebase. The implementation is faithful to the design with some improvements (pipeline abstraction, metrics, region auto-detection). One bug found, several coverage gaps filled with tests, design doc updated to match reality.

## Bug: `CompressedSize` metric always 0 on success

**Status: FIXED** (commit 1cccd260)

In `process_segments`, after the pipeline completes successfully:

```rust
// S3PipelineUploader::process does: let bytes = std::mem::take(&mut data.bytes);
// ... upload happens ...
// Back in process_segments:
data.metrics.compressed_size = Some(data.bytes.len() as u64);  // always 0!
```

`GzipCompressor` correctly sets `data.metrics.compressed_size` to the compressed size. Then `S3PipelineUploader` takes ownership of `data.bytes` via `mem::take` (leaving an empty vec). The final line in `process_segments` overwrites the correct value with `data.bytes.len()` which is now 0.

**Fix:** Remove the final `data.metrics.compressed_size = ...` line — the gzip stage already sets it correctly.

**Test:** `compressed_size_metric_is_nonzero_after_pipeline` — confirmed failing.

## New metric: `InvalidFileHeader`

**Status: implemented**

Added `invalid_file_header: bool` to `SegmentProcessMetrics`. When the worker can't parse a `SegmentMetadata` header from a segment file (corrupted, truncated, wrong format), this metric is `true`. The file is still uploaded — the worker stays a dumb pipe — but operators can alarm on this metric to detect problems.

`SealedSegment::creation_epoch_secs` now returns `(u64, bool)` where the bool indicates whether the header was valid.

## Tests added

| Test | File | What it covers |
|------|------|----------------|
| `compressed_size_metric_is_nonzero_after_pipeline` | mod.rs | Bug #1 — RED, proves the metric is 0 |
| `worker_loop_drains_on_stop` | mod.rs | WorkerLoop processes remaining segments when stop signal is set |
| `worker_loop_continues_after_processor_error` | mod.rs | WorkerLoop skips failed segment, continues to next |
| `uncompressed_size_matches_bytes_len` | mod.rs | Documents that metadata().len() == bytes.len() (redundancy) |
| `upload_failure_does_not_delete_local_file` | s3.rs | **Rewritten** — old test was a no-op (never called upload_and_delete). New test destroys the S3 backend and verifies the local file survives |
| `object_key_epoch_secs_fallback_to_zero_produces_1970_path` | s3.rs | Documents silent fallback when epoch_secs missing |
| `object_key_epoch_secs_unparseable_falls_back_to_zero` | s3.rs | Documents silent fallback when epoch_secs is garbage |
| `trace_stem_normal_path` | mod.rs | Documents trace_stem for `/tmp/traces/trace.bin` → `"trace"` |
| `trace_stem_directory_path` | mod.rs | Documents trace_stem for `/tmp/traces/` → `"traces"` |
| `trace_stem_root_path` | mod.rs | Documents trace_stem for `/` → `"trace"` (fallback) |
| `trace_dir_for_directory_path` | mod.rs | Documents trace_dir for `/tmp/traces/` → `/tmp` |
| Segment index contiguity check | s3_integration.rs | Added invariant #4 to stress test (was listed in comments but never implemented) |
| `InvalidFileHeader` metric assertion | pipeline_metrics.rs | Verifies the new metric key is emitted |

## Design doc updated

`design/s3-worker-design.md` updated to match the implementation:

- Architecture diagram now shows the `SegmentProcessor` pipeline
- Documented the dedicated `current_thread` tokio runtime on the worker thread
- `UploaderConfig` → `BackgroundTaskConfig`
- `.shutdown` sentinel → `AtomicBool` stop signal
- Added sections: pipeline abstraction, region auto-detection, custom `S3KeyFn`, metrics
- Graceful shutdown section reflects actual `AtomicBool` + thread join mechanism
- Testing strategy updated to reflect actual test coverage
- Removed metrics from "Future Work" (already implemented)

## Remaining items (not yet fixed)

| # | Issue | Severity | Notes |
|---|-------|----------|-------|
| 2 | `#[allow(dead_code)]` on `parse_segment_timestamp` | ~~Cleanup~~ | ✅ Fixed — removed unnecessary allow |
| 3 | Circuit breaker skip logged as warning | Low | Noisy during outages. Could add `ProcessErrorKind::CircuitBreakerOpen` or log at debug |
| 8 | `S3Uploader` is `pub` but only used internally | Low | Should be `pub(crate)` |
| 10 | Redundant `metadata()` call in `process_segments` | Low | `uncompressed_size` could use `bytes.len()` instead of a separate `fs::metadata` call |

## Fixes applied

| # | Issue | Fix |
|---|-------|-----|
| 1 | `CompressedSize` metric always 0 | Removed the overwrite line in `process_segments` — GzipCompressor already sets it correctly |
| 2 | `#[allow(dead_code)]` on `parse_segment_timestamp` | Removed — method is used |
| — | Clippy: derivable Default on PipelineMetrics | `#[derive(Default)]` |
| — | Clippy: `io_other_error` | `std::io::Error::other()` |
| — | Clippy: collapsible if chains in s3_stress_test | Collapsed with let-chains |
| — | Stress test invariant 4 | Changed from contiguity to no-duplicates (gaps expected from eviction) |
