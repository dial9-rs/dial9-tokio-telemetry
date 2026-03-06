//! Sealed-file detection for the worker pipeline.
//!
//! Finds `.bin` files produced by `RotatingWriter` rename-on-seal,
//! ignoring `.active` files that are still being written.

use std::path::{Path, PathBuf};

/// A sealed trace segment ready for processing.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SealedSegment {
    pub path: PathBuf,
    pub index: u32,
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
        if path.extension().map_or(true, |ext| ext != "bin") {
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
        assert_eq!(segments.len(), 2);
        assert_eq!(segments[0].index, 0);
        assert_eq!(segments[1].index, 1);
    }

    #[test]
    fn sorted_oldest_first() {
        let dir = TempDir::new().unwrap();
        touch(dir.path(), "trace.5.bin");
        touch(dir.path(), "trace.2.bin");
        touch(dir.path(), "trace.9.bin");

        let segments = find_sealed_segments(dir.path(), "trace").unwrap();
        let indices: Vec<u32> = segments.iter().map(|s| s.index).collect();
        assert_eq!(indices, vec![2, 5, 9]);
    }

    #[test]
    fn ignores_unrelated_files() {
        let dir = TempDir::new().unwrap();
        touch(dir.path(), "trace.0.bin");
        touch(dir.path(), "other.0.bin");
        touch(dir.path(), "trace.txt");
        touch(dir.path(), "readme.md");

        let segments = find_sealed_segments(dir.path(), "trace").unwrap();
        assert_eq!(segments.len(), 1);
        assert_eq!(segments[0].index, 0);
    }

    #[test]
    fn empty_directory() {
        let dir = TempDir::new().unwrap();
        let segments = find_sealed_segments(dir.path(), "trace").unwrap();
        assert!(segments.is_empty());
    }

    #[test]
    fn parse_segment_index_valid() {
        assert_eq!(parse_segment_index("trace.0.bin", "trace"), Some(0));
        assert_eq!(parse_segment_index("trace.42.bin", "trace"), Some(42));
        assert_eq!(parse_segment_index("my-app.100.bin", "my-app"), Some(100));
    }

    #[test]
    fn parse_segment_index_invalid() {
        assert_eq!(parse_segment_index("trace.0.bin.active", "trace"), None);
        assert_eq!(parse_segment_index("trace.bin", "trace"), None);
        assert_eq!(parse_segment_index("other.0.bin", "trace"), None);
        assert_eq!(parse_segment_index("trace.abc.bin", "trace"), None);
    }
}
