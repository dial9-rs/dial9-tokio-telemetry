//! Sealed-file detection for the worker pipeline.
//!
//! Finds `.bin` files produced by `RotatingWriter` rename-on-seal,
//! ignoring `.active` files that are still being written.

use crate::telemetry::events::TelemetryEvent;
use crate::telemetry::format;
use std::fs::File;
use std::io::BufReader;
use std::path::{Path, PathBuf};

/// A sealed trace segment ready for processing.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SealedSegment {
    pub(crate) path: PathBuf,
    pub(crate) index: u32,
}

impl SealedSegment {
    /// Segment creation time as epoch seconds, parsed from SegmentMetadata header.
    /// Falls back to file mtime if header parsing fails, then current time if mtime is unavailable.
    /// Returns `(epoch_secs, header_valid)`.
    pub(crate) fn creation_epoch_secs(&self) -> (u64, bool) {
        // First try to parse timestamp from SegmentMetadata header
        if let Ok(timestamp_nanos) = self.parse_segment_timestamp() {
            return (timestamp_nanos / 1_000_000_000, true);
        }

        // Fall back to file mtime
        let secs = std::fs::metadata(&self.path)
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

    /// Parse the timestamp from the SegmentMetadata header in the segment file.
    /// Returns the timestamp in nanoseconds if successful.
    fn parse_segment_timestamp(&self) -> std::io::Result<u64> {
        let file = File::open(&self.path)?;
        let mut reader = BufReader::new(file);

        // Read and validate header
        let (_magic, _version) = format::read_header(&mut reader)?;

        // Try to read the first event, which should be SegmentMetadata
        if let Some(event) = format::read_event(&mut reader)?
            && let TelemetryEvent::SegmentMetadata {
                timestamp_nanos, ..
            } = event
        {
            return Ok(timestamp_nanos);
        }

        Err(std::io::Error::new(
            std::io::ErrorKind::InvalidData,
            "No SegmentMetadata found at start of segment",
        ))
    }
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
        use crate::telemetry::events::TelemetryEvent;
        use crate::telemetry::writer::{RotatingWriter, TraceWriter};
        use tempfile::TempDir;

        let dir = TempDir::new().unwrap();
        let base = dir.path().join("trace");

        let mut writer = RotatingWriter::single_file(&base).unwrap();
        writer
            .set_segment_metadata(vec![("test".into(), "value".into())])
            .unwrap();

        let event = TelemetryEvent::WorkerPark {
            timestamp_nanos: 1000000000,
            worker_id: 0,
            worker_local_queue_depth: 0,
            cpu_time_nanos: 0,
        };
        writer.write_event(&event).unwrap();
        writer.flush().unwrap();

        let segment = SealedSegment {
            path: base.clone(),
            index: 0,
        };

        let timestamp_nanos = segment.parse_segment_timestamp().unwrap();

        let now_nanos = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos() as u64;
        let diff = now_nanos.abs_diff(timestamp_nanos);

        check!(diff < 60_000_000_000);
    }

    #[test]
    fn test_creation_epoch_secs_uses_parsed_timestamp() {
        use crate::telemetry::events::TelemetryEvent;
        use crate::telemetry::writer::{RotatingWriter, TraceWriter};
        use tempfile::TempDir;

        let dir = TempDir::new().unwrap();
        let base = dir.path().join("trace");

        let mut writer = RotatingWriter::single_file(&base).unwrap();
        writer
            .set_segment_metadata(vec![("test".into(), "value".into())])
            .unwrap();

        let event = TelemetryEvent::WorkerPark {
            timestamp_nanos: 1000000000,
            worker_id: 0,
            worker_local_queue_depth: 0,
            cpu_time_nanos: 0,
        };
        writer.write_event(&event).unwrap();
        writer.flush().unwrap();

        let segment = SealedSegment {
            path: base.clone(),
            index: 0,
        };

        let (epoch_secs, header_valid) = segment.creation_epoch_secs();
        check!(header_valid);
        let expected_secs = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_secs();

        let diff = expected_secs.abs_diff(epoch_secs);

        check!(diff < 60);
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
