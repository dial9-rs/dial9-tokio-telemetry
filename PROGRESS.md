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
