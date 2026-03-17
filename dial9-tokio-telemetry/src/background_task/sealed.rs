//! Sealed-file detection for the worker pipeline.
//!
//! Finds `.bin` files produced by `RotatingWriter` rename-on-seal,
//! ignoring `.active` files that are still being written.

use std::path::{Path, PathBuf};

/// A sealed trace segment ready for processing.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SealedSegment {
    pub(crate) path: PathBuf,
    pub(crate) index: u32,
}

/// Segment creation time as epoch seconds, parsed from SegmentMetadata header.
/// Returns `(epoch_secs, true)` if the header was valid, or falls back to
/// file mtime / current time with `(epoch_secs, false)`.
pub(crate) fn creation_epoch_secs(data: &[u8], path: &Path) -> (u64, bool) {
    if let Some(ts) = parse_segment_timestamp(data) {
        return (ts / 1_000_000_000, true);
    }
    let secs = std::fs::metadata(path)
        .and_then(|m| m.modified())
        .ok()
        .and_then(|t| t.duration_since(std::time::UNIX_EPOCH).ok())
        .map(|d| d.as_secs())
        .unwrap_or_else(|| {
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap_or_default()
                .as_secs()
        });
    (secs, false)
}

/// Parse the timestamp (nanos) from the first SegmentMetadata event in a trace segment.
fn parse_segment_timestamp(data: &[u8]) -> Option<u64> {
    use dial9_trace_format::decoder::{DecodedFrameRef, Decoder};

    let mut dec = Decoder::new(data)?;
    let mut events_seen = 0;
    while let Ok(Some(frame)) = dec.next_frame_ref() {
        if let DecodedFrameRef::Event {
            type_id,
            timestamp_ns,
            ..
        } = frame
        {
            events_seen += 1;
            let name = dec.registry().get(type_id).map(|s| s.name.as_str())?;
            if name == "SegmentMetadataEvent" {
                return timestamp_ns;
            }
            if events_seen >= 10 {
                return None;
            }
        }
    }
    None
}

/// Find sealed `.bin` segments in `dir`, sorted oldest-first by index.
///
/// Matches files named `{stem}.{index}.bin` where `stem` matches the
/// given base path's file stem. Ignores `.active` files and any files
/// that don't match the expected naming pattern.
pub fn find_sealed_segments(dir: &Path, stem: &str) -> std::io::Result<Vec<SealedSegment>> {
    let mut segments = Vec::new();
    for entry in std::fs::read_dir(dir)? {
        let entry = entry?;
        let path = entry.path();
        if path.extension().is_none_or(|ext| ext != "bin") {
            continue;
        }
        // Guard against .bin.active being misread — those have extension "active"
        // so the check above already excludes them, but be explicit.
        let file_name = match path.file_name().and_then(|n| n.to_str()) {
            Some(n) => n,
            None => continue,
        };
        if let Some(index) = parse_segment_index(file_name, stem) {
            segments.push(SealedSegment { path, index });
        }
    }
    segments.sort_by_key(|s| s.index);
    Ok(segments)
}

/// Parse segment index from a filename like `trace.3.bin`.
/// Returns `None` if the filename doesn't match `{stem}.{index}.bin`.
fn parse_segment_index(file_name: &str, stem: &str) -> Option<u32> {
    let rest = file_name.strip_prefix(stem)?.strip_prefix('.')?;
    let index_str = rest.strip_suffix(".bin")?;
    index_str.parse().ok()
}

#[cfg(test)]
mod tests {
    use super::*;
    use assert2::check;
    use std::fs::File;
    use tempfile::TempDir;

    fn touch(dir: &Path, name: &str) {
        File::create(dir.join(name)).unwrap();
    }

    #[test]
    fn finds_sealed_ignores_active() {
        let dir = TempDir::new().unwrap();
        touch(dir.path(), "trace.0.bin");
        touch(dir.path(), "trace.1.bin");
        touch(dir.path(), "trace.2.bin.active");

        let segments = find_sealed_segments(dir.path(), "trace").unwrap();
        check!(segments.len() == 2);
        check!(segments[0].index == 0);
        check!(segments[1].index == 1);
    }

    #[test]
    fn sorted_oldest_first() {
        let dir = TempDir::new().unwrap();
        touch(dir.path(), "trace.5.bin");
        touch(dir.path(), "trace.2.bin");
        touch(dir.path(), "trace.9.bin");

        let segments = find_sealed_segments(dir.path(), "trace").unwrap();
        let indices: Vec<u32> = segments.iter().map(|s| s.index).collect();
        check!(indices == [2, 5, 9]);
    }

    #[test]
    fn ignores_unrelated_files() {
        let dir = TempDir::new().unwrap();
        touch(dir.path(), "trace.0.bin");
        touch(dir.path(), "other.0.bin");
        touch(dir.path(), "trace.txt");
        touch(dir.path(), "readme.md");

        let segments = find_sealed_segments(dir.path(), "trace").unwrap();
        check!(segments.len() == 1);
        check!(segments[0].index == 0);
    }

    #[test]
    fn empty_directory() {
        let dir = TempDir::new().unwrap();
        let segments = find_sealed_segments(dir.path(), "trace").unwrap();
        check!(segments.is_empty());
    }

    #[test]
    fn test_parse_segment_timestamp() {
        use crate::telemetry::events::RawEvent;
        use crate::telemetry::writer::{RotatingWriter, TraceWriter};
        use tempfile::TempDir;

        let dir = TempDir::new().unwrap();
        let base = dir.path().join("trace");

        let mut writer = RotatingWriter::single_file(&base).unwrap();
        writer
            .set_segment_metadata(vec![("test".into(), "value".into())])
            .unwrap();

        let event = RawEvent::WorkerPark {
            timestamp_nanos: 1000000000,
            worker_id: crate::telemetry::format::WorkerId::from(0usize),
            worker_local_queue_depth: 0,
            cpu_time_nanos: 0,
        };
        writer.write_event(&event).unwrap();
        writer.flush().unwrap();

        let data = std::fs::read(&base).unwrap();
        let timestamp_nanos = parse_segment_timestamp(&data).unwrap();

        let now_nanos = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos() as u64;
        let diff = now_nanos.abs_diff(timestamp_nanos);

        check!(diff < 60_000_000_000);
    }

    #[test]
    fn test_creation_epoch_secs_uses_parsed_timestamp() {
        use crate::telemetry::events::RawEvent;
        use crate::telemetry::writer::{RotatingWriter, TraceWriter};
        use tempfile::TempDir;

        let dir = TempDir::new().unwrap();
        let base = dir.path().join("trace");

        let mut writer = RotatingWriter::single_file(&base).unwrap();
        writer
            .set_segment_metadata(vec![("test".into(), "value".into())])
            .unwrap();

        let event = RawEvent::WorkerPark {
            timestamp_nanos: 1000000000,
            worker_id: crate::telemetry::format::WorkerId::from(0usize),
            worker_local_queue_depth: 0,
            cpu_time_nanos: 0,
        };
        writer.write_event(&event).unwrap();
        writer.flush().unwrap();

        let data = std::fs::read(&base).unwrap();
        let (epoch_secs, header_valid) = creation_epoch_secs(&data, &base);
        check!(header_valid);
        let expected_secs = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_secs();

        let diff = expected_secs.abs_diff(epoch_secs);

        check!(diff < 60);
    }

    #[test]
    fn test_creation_epoch_secs_invalid_data_falls_back() {
        let dir = tempfile::TempDir::new().unwrap();
        let path = dir.path().join("trace.0.bin");
        std::fs::write(&path, b"not a valid trace").unwrap();

        let data = std::fs::read(&path).unwrap();
        let (epoch_secs, header_valid) = creation_epoch_secs(&data, &path);
        check!(!header_valid);
        // Should fall back to mtime, which should be recent
        let now = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_secs();
        check!(now.abs_diff(epoch_secs) < 60);
    }

    #[test]
    fn parse_segment_index_valid() {
        check!(parse_segment_index("trace.0.bin", "trace") == Some(0));
        check!(parse_segment_index("trace.42.bin", "trace") == Some(42));
        check!(parse_segment_index("my-app.100.bin", "my-app") == Some(100));
    }

    #[test]
    fn parse_segment_index_invalid() {
        check!(parse_segment_index("trace.0.bin.active", "trace") == None);
        check!(parse_segment_index("trace.bin", "trace") == None);
        check!(parse_segment_index("other.0.bin", "trace") == None);
        check!(parse_segment_index("trace.abc.bin", "trace") == None);
    }
}
