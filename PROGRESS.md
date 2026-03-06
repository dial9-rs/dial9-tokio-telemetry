# Workstream A Progress

## A1: Remove SimpleBinaryWriter
- **Status**: ✅ Complete
- **Summary**: Removed `SimpleBinaryWriter`, added `RotatingWriter::single_file()` constructor. Updated 16 files. All tests pass.

## A2: RotatingWriter rename-on-seal
- **Status**: ✅ Complete
- **Summary**: `RotatingWriter::new()` now creates files with `.bin.active` suffix while writing. On rotation, the previous file is renamed `.active` → `.bin` (atomic on Linux). Added `seal()` to `TraceWriter` trait (default no-op) and implemented it for `RotatingWriter`. `TelemetryGuard::drop` calls `seal()` after final flush. `single_file()` is unchanged (no `.active` suffix). 4 new tests added, all 69 lib tests + integration tests pass.
- **Key changes**:
  - `writer.rs`: `active_path()` helper, `new()` uses `.active` suffix, `rotate()` renames on seal, `seal()` impl
  - `TraceWriter` trait: new `seal()` method with default no-op
  - `recorder/mod.rs`: `TelemetryRecorder::seal()`, `TelemetryGuard::drop` calls seal
  - `recorder/event_writer.rs`: `EventWriter::seal()` forwarding

## A3: SegmentMetadata event
- **Status**: ✅ Complete
- **Summary**: Added `SegmentMetadata` event variant (wire code 11, format v14). `RotatingWriter::set_segment_metadata()` writes key-value metadata at the start of each segment (initial + every rotation). Wire format: `code(u8) + num_entries(u16) + (key_len(u16) + key + val_len(u16) + val)*`. Updated events.rs, format.rs, writer.rs, analysis.rs, trace_to_fat_jsonl example, and JS trace viewer parser. 5 new tests, all 82 lib + integration tests pass (12 stress iterations clean).
- **Key changes**:
  - `events.rs`: `SegmentMetadata { entries: Vec<(String, String)> }` variant, updated match arms
  - `format.rs`: wire code 11, write/read support, version bump 13→14
  - `writer.rs`: `set_segment_metadata()`, `write_segment_metadata()` helper, called in `rotate()`
  - `analysis.rs`: added to match arm
  - `trace_to_fat_jsonl.rs`: added to match arm
  - `trace_viewer/trace_parser.js`: wire code 11 parsing, version range bump

## A4: Sealed-file watcher
- **Status**: ✅ Complete
- **Summary**: Added `worker::sealed` module with `find_sealed_segments()` function. Scans a directory for `.bin` files matching `{stem}.{index}.bin` pattern, ignores `.active` files, returns sorted oldest-first by index. Created `src/worker/mod.rs` and `src/worker/sealed.rs`, registered in `lib.rs`. 6 new tests, all 88 tests pass (14 stress iterations clean, excluding pre-existing flaky `cpu_sample_timestamps_align_with_wall_clock`).
- **Key changes**:
  - `src/worker/mod.rs`: new module
  - `src/worker/sealed.rs`: `SealedSegment` struct, `find_sealed_segments()`, `parse_segment_index()`
  - `src/lib.rs`: registered `worker` module

## A5: S3 uploader with transfer manager
- **Status**: ✅ Complete
- **Summary**: Added `worker-s3` feature flag with `aws-sdk-s3-transfer-manager`, `aws-config`, and `flate2` as optional deps. Implemented `S3Uploader` with `S3Config` builder (bucket, prefix, service_name, instance_path), `gzip_compress` free function, `object_key` on `S3Config`, and `upload_and_delete` async method using the transfer manager. 11 tests: 9 unit tests covering key format, gzip roundtrip, builder validation + 3 s3s-backed integration tests covering upload+delete, gzip data roundtrip through S3, and failure-preserves-local-file. All 99 tests pass (stress clean, excluding pre-existing flaky `cpu_sample_timestamps_align_with_wall_clock`).
- **Key changes**:
  - `Cargo.toml`: `worker = []` and `worker-s3 = ["worker", ...]` feature flags; optional deps for aws-sdk-s3-transfer-manager, aws-config, flate2; dev-deps for s3s, s3s-fs, s3s-aws, aws-sdk-s3
  - `src/worker/s3.rs`: `S3Config` builder, `S3Uploader`, `gzip_compress()`, `upload_and_delete()`, s3s integration tests
  - `src/worker/mod.rs`: registered `s3` module behind `worker-s3` feature
