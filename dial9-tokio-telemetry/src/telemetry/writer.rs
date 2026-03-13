use crate::telemetry::events::{RawEvent, TelemetryEvent};
use crate::telemetry::format;
use crate::telemetry::recorder::flush_state::FlushState;
use std::collections::VecDeque;
use std::fs::{self, File};
use std::io::{BufWriter, Write};
use std::path::{Path, PathBuf};

pub trait TraceWriter: Send {
    fn write_event(&mut self, event: &RawEvent) -> std::io::Result<()>;
    fn flush(&mut self) -> std::io::Result<()>;
    /// Returns true if the writer rotated to a new file since the last call to this method.
    /// Used by the flush path to know when to re-emit SpawnLocationDefs.
    fn take_rotated(&mut self) -> bool {
        false
    }
    /// Write a group of events atomically — all events in the batch land in the
    /// same file. The file may exceed `max_file_size` after this call; rotation
    /// is deferred until the entire batch is written.
    fn write_atomic(&mut self, events: &[RawEvent]) -> std::io::Result<()> {
        for event in events {
            self.write_event(event)?;
        }
        Ok(())
    }
    /// Seal the current segment: flush and rename `.active` → `.bin`.
    /// Called on shutdown so the final segment is visible to the worker.
    fn seal(&mut self) -> std::io::Result<()> {
        self.flush()
    }
}

impl<W: TraceWriter + ?Sized> TraceWriter for Box<W> {
    fn write_event(&mut self, event: &RawEvent) -> std::io::Result<()> {
        (**self).write_event(event)
    }
    fn flush(&mut self) -> std::io::Result<()> {
        (**self).flush()
    }
    fn take_rotated(&mut self) -> bool {
        (**self).take_rotated()
    }
    fn write_atomic(&mut self, events: &[RawEvent]) -> std::io::Result<()> {
        (**self).write_atomic(events)
    }
    fn seal(&mut self) -> std::io::Result<()> {
        (**self).seal()
    }
}

/// A writer that discards all events. Useful for benchmarking hook overhead
/// without I/O costs.
pub struct NullWriter;

impl TraceWriter for NullWriter {
    fn write_event(&mut self, _event: &RawEvent) -> std::io::Result<()> {
        Ok(())
    }
    fn flush(&mut self) -> std::io::Result<()> {
        Ok(())
    }
}

/// Resolves [`RawEvent`]s into [`TelemetryEvent`]s by interning spawn locations.
///
/// This is useful for custom [`TraceWriter`] implementations (e.g. in tests)
/// that need to convert raw events into their wire format. The resolver tracks
/// which spawn-location definitions have been emitted and re-emits them as
/// needed.
///
/// Call [`on_rotate`](Self::on_rotate) when the writer rotates to a new file
/// so that definitions are re-emitted in the new file.
pub struct EventResolver {
    inner: FlushState,
}

impl Default for EventResolver {
    fn default() -> Self {
        Self::new()
    }
}

impl EventResolver {
    /// Create a new resolver with empty interning state.
    pub fn new() -> Self {
        Self {
            inner: FlushState::new(),
        }
    }

    /// Resolve a [`RawEvent`] into one or more [`TelemetryEvent`]s.
    ///
    /// For events that reference a spawn location, this returns the
    /// `SpawnLocationDef` (if not yet emitted in the current file) followed
    /// by the event itself.
    pub fn resolve(&mut self, raw: &RawEvent) -> smallvec::SmallVec<[TelemetryEvent; 3]> {
        self.inner.resolve(raw)
    }

    /// Notify the resolver that the writer rotated to a new file.
    /// The next reference to any spawn location will re-emit its definition.
    pub fn on_rotate(&mut self) {
        self.inner.on_rotate();
    }
}

/// A writer that rotates trace files to bound disk usage.
///
/// - `max_file_size`: rotate to a new file when the current file exceeds this size
/// - `max_total_size`: delete oldest files when total size across all files exceeds this
///
/// Files are named `{base_path}.0.bin`, `{base_path}.1.bin`, etc.
/// Each file is a self-contained trace with its own header.
pub struct RotatingWriter {
    base_path: PathBuf,
    max_file_size: u64,
    max_total_size: u64,
    /// Tracks (path, size) of files oldest-first.
    files: VecDeque<(PathBuf, u64)>,
    total_size: u64,
    current_writer: BufWriter<File>,
    current_size: u64,
    next_index: u32,
    /// Set when we've hit the total size cap; silently drops further events.
    stopped: bool,
    /// Set to true when a rotation occurs; cleared by `take_rotated`.
    rotated: bool,
    /// Optional metadata written at the start of each segment.
    segment_metadata: Option<Vec<(String, String)>>,
    /// Interning state for spawn locations.
    flush_state: FlushState,
}

impl RotatingWriter {
    pub fn new(
        base_path: impl Into<PathBuf>,
        max_file_size: u64,
        max_total_size: u64,
    ) -> std::io::Result<Self> {
        let base_path = base_path.into();
        if let Some(parent) = base_path.parent() {
            fs::create_dir_all(parent)?;
        }
        let first_path = Self::active_path(&base_path, 0);
        let file = File::create(&first_path)?;
        let mut writer = BufWriter::new(file);
        format::write_header(&mut writer)?;
        let header_size = format::HEADER_SIZE as u64;

        let mut files = VecDeque::new();
        files.push_back((first_path, header_size));

        Ok(Self {
            base_path,
            max_file_size,
            max_total_size,
            files,
            total_size: header_size,
            current_writer: writer,
            current_size: header_size,
            next_index: 1,
            stopped: false,
            rotated: false,
            segment_metadata: None,
            flush_state: FlushState::new(),
        })
    }

    /// Create a writer that writes to a single file with no rotation or eviction.
    /// The file is created at exactly the given path.
    ///
    /// This must not be used with rotation — the `u64::MAX` size limits prevent
    /// rotation in practice, but if it were triggered, `file_path()` would produce
    /// indexed names (e.g. `trace.0.bin`) derived from the stem, which would be
    /// surprising for a "single file" writer.
    ///
    /// Note: this intentionally duplicates setup from `new()` rather than delegating
    /// to it, because `new()` writes to an indexed path (`base.0.bin`) while
    /// `single_file()` writes to the exact path given.
    pub fn single_file(path: impl Into<PathBuf>) -> std::io::Result<Self> {
        let path = path.into();
        if let Some(parent) = path.parent() {
            fs::create_dir_all(parent)?;
        }
        let file = File::create(&path)?;
        let mut writer = BufWriter::new(file);
        format::write_header(&mut writer)?;
        let header_size = format::HEADER_SIZE as u64;

        let mut files = VecDeque::new();
        files.push_back((path.clone(), header_size));

        Ok(Self {
            base_path: path,
            max_file_size: u64::MAX,
            max_total_size: u64::MAX,
            files,
            total_size: header_size,
            current_writer: writer,
            current_size: header_size,
            next_index: 1,
            stopped: false,
            rotated: false,
            segment_metadata: None,
            flush_state: FlushState::new(),
        })
    }

    /// Set key-value metadata to be written at the start of each new segment.
    /// Immediately writes metadata to the current segment (call before writing events).
    pub fn set_segment_metadata(&mut self, entries: Vec<(String, String)>) -> std::io::Result<()> {
        self.segment_metadata = Some(entries);
        self.write_segment_metadata()
    }

    /// Write the segment metadata event if configured. Called after writing the header.
    fn write_segment_metadata(&mut self) -> std::io::Result<()> {
        if let Some(ref entries) = self.segment_metadata {
            let timestamp_nanos = std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap_or_default()
                .as_nanos() as u64;
            let event = TelemetryEvent::SegmentMetadata {
                timestamp_nanos,
                entries: entries.clone(),
            };
            let size = format::wire_event_size(&event) as u64;
            format::write_event(&mut self.current_writer, &event)?;
            self.current_size += size;
            self.total_size += size;
            if let Some(last) = self.files.back_mut() {
                last.1 = self.current_size;
            }
        }
        Ok(())
    }

    fn file_path(base: &Path, index: u32) -> PathBuf {
        let stem = base.file_stem().unwrap_or_default().to_string_lossy();
        let parent = base.parent().unwrap_or(Path::new("."));
        parent.join(format!("{}.{}.bin", stem, index))
    }

    /// Path for a segment that is actively being written.
    fn active_path(base: &Path, index: u32) -> PathBuf {
        let stem = base.file_stem().unwrap_or_default().to_string_lossy();
        let parent = base.parent().unwrap_or(Path::new("."));
        parent.join(format!("{}.{}.bin.active", stem, index))
    }

    fn rotate(&mut self) -> std::io::Result<()> {
        self.current_writer.flush()?;
        // Seal the current segment: rename .active → .bin
        if let Some(last) = self.files.back_mut() {
            last.1 = self.current_size;
            let sealed = Self::file_path(&self.base_path, self.next_index - 1);
            fs::rename(&last.0, &sealed)?;
            last.0 = sealed;
        }

        let new_path = Self::active_path(&self.base_path, self.next_index);
        self.next_index += 1;
        let file = File::create(&new_path)?;
        self.current_writer = BufWriter::new(file);
        format::write_header(&mut self.current_writer)?;
        let header_size = format::HEADER_SIZE as u64;
        self.current_size = header_size;
        self.total_size += header_size;
        self.files.push_back((new_path, header_size));
        self.write_segment_metadata()?;
        self.rotated = true;
        self.flush_state.on_rotate();

        tracing::info!(
            segment_index = self.next_index - 1,
            "rotated to new trace segment"
        );
        self.evict_oldest()?;
        Ok(())
    }

    fn evict_oldest(&mut self) -> std::io::Result<()> {
        // Always keep at least the current file.
        while self.total_size > self.max_total_size && self.files.len() > 1 {
            if let Some((path, size)) = self.files.pop_front() {
                self.total_size -= size;
                if let Err(e) = fs::remove_file(&path) {
                    // NotFound is expected when the S3 worker already deleted the file.
                    if e.kind() != std::io::ErrorKind::NotFound {
                        tracing::warn!("failed to evict old trace segment {}: {e}", path.display());
                    }
                }
            }
        }
        // If even the current file alone exceeds total budget, stop writing.
        if self.total_size > self.max_total_size {
            self.stopped = true;
        }
        Ok(())
    }

    /// Resolve a RawEvent and write it. Rotation is deferred until after
    /// the complete logical unit (defs + event) is written, so they always
    /// land in the same file.
    fn write_resolved(&mut self, event: &RawEvent) -> std::io::Result<()> {
        let resolved = self.flush_state.resolve(event);
        for e in &resolved {
            self.write_event_inner(e)?;
        }
        self.maybe_rotate()?;
        Ok(())
    }

    fn write_event_inner(&mut self, event: &TelemetryEvent) -> std::io::Result<()> {
        if self.stopped {
            return Ok(());
        }
        let event_size = format::wire_event_size(event) as u64;
        format::write_event(&mut self.current_writer, event)?;
        self.current_size += event_size;
        self.total_size += event_size;
        // Update tracked size for current file.
        if let Some(last) = self.files.back_mut() {
            last.1 = self.current_size;
        }
        // Check if we've exceeded budget even without rotation
        if self.total_size > self.max_total_size {
            self.current_writer.flush()?;
            self.evict_oldest()?;
        }
        Ok(())
    }

    /// Rotate if the current file exceeds max_file_size.
    /// Called after writing a complete logical unit (def + event).
    fn maybe_rotate(&mut self) -> std::io::Result<()> {
        if !self.stopped && self.current_size > self.max_file_size {
            self.rotate()?;
        }
        Ok(())
    }
}

impl TraceWriter for RotatingWriter {
    fn write_event(&mut self, event: &RawEvent) -> std::io::Result<()> {
        self.write_resolved(event)
    }

    fn flush(&mut self) -> std::io::Result<()> {
        if !self.stopped {
            self.current_writer.flush()?;
        }
        Ok(())
    }

    fn take_rotated(&mut self) -> bool {
        std::mem::replace(&mut self.rotated, false)
    }

    fn write_atomic(&mut self, events: &[RawEvent]) -> std::io::Result<()> {
        for event in events {
            // Write without rotating — rotation is deferred until the
            // entire atomic batch is written.
            let resolved = self.flush_state.resolve(event);
            for e in &resolved {
                self.write_event_inner(e)?;
            }
        }
        self.maybe_rotate()?;
        Ok(())
    }

    fn seal(&mut self) -> std::io::Result<()> {
        self.flush()?;
        // Rename .active → .bin for the current segment (if it has .active suffix)
        if let Some(last) = self.files.back_mut()
            && last.0.extension().is_some_and(|ext| ext == "active")
        {
            let sealed = Self::file_path(&self.base_path, self.next_index - 1);
            fs::rename(&last.0, &sealed)?;
            last.0 = sealed;
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::telemetry::analysis::TraceReader;
    use crate::telemetry::format;
    use std::io::Read;
    use tempfile::TempDir;

    fn park_event() -> RawEvent {
        RawEvent::WorkerPark {
            timestamp_nanos: 1000,
            worker_id: 0,
            worker_local_queue_depth: 2,
            cpu_time_nanos: 0,
        }
    }

    fn rotating_file(base: &std::path::Path, i: u32) -> String {
        format!("{}.{}.bin", base.display(), i)
    }

    /// Read all events from a trace file, asserting the header is valid.
    fn read_trace_events(path: &str) -> Vec<TelemetryEvent> {
        let mut reader = TraceReader::new(path).unwrap();
        let (magic, version) = reader.read_header().unwrap();
        assert_eq!(magic, "TOKIOTRC");
        assert_eq!(version, format::VERSION);
        reader.read_all().unwrap()
    }

    /// Total size of all trace files (.bin and .active) in a directory.
    fn total_disk_usage(dir: &std::path::Path) -> u64 {
        std::fs::read_dir(dir)
            .unwrap()
            .filter_map(|e| e.ok())
            .filter(|e| {
                let p = e.path();
                p.extension()
                    .is_some_and(|ext| ext == "bin" || ext == "active")
            })
            .map(|e| e.metadata().unwrap().len())
            .sum()
    }

    #[test]
    fn test_writer_creation() {
        let dir = TempDir::new().unwrap();
        let path = dir.path().join("test_trace_v2.bin");
        let writer = RotatingWriter::single_file(&path);
        assert!(writer.is_ok());
    }

    #[test]
    fn test_write_event() {
        let dir = TempDir::new().unwrap();
        let path = dir.path().join("test_event_v2.bin");
        let mut writer = RotatingWriter::single_file(&path).unwrap();

        writer.write_event(&park_event()).unwrap();
        writer.flush().unwrap();

        let metadata = std::fs::metadata(&path).unwrap();
        // WorkerPark is 11 bytes on the wire
        let expected = format::HEADER_SIZE as u64 + 11;
        assert_eq!(metadata.len(), expected);
    }

    #[test]
    fn test_write_batch_sizes() {
        let dir = TempDir::new().unwrap();
        let path = dir.path().join("test_batch_v2.bin");
        let mut writer = RotatingWriter::single_file(&path).unwrap();

        let events = vec![
            RawEvent::WorkerPark {
                timestamp_nanos: 1000,
                worker_id: 0,
                worker_local_queue_depth: 2,
                cpu_time_nanos: 0,
            }, // 11 bytes
            RawEvent::WorkerPark {
                timestamp_nanos: 2000,
                worker_id: 0,
                worker_local_queue_depth: 2,
                cpu_time_nanos: 0,
            }, // 11 bytes
        ];

        for e in &events {
            writer.write_event(e).unwrap();
        }
        writer.flush().unwrap();

        let metadata = std::fs::metadata(&path).unwrap();
        assert_eq!(metadata.len(), (format::HEADER_SIZE + 11 + 11) as u64);
    }

    #[test]
    fn test_binary_format_header() {
        let dir = TempDir::new().unwrap();
        let path = dir.path().join("test_format_v2.bin");
        let writer = RotatingWriter::single_file(&path).unwrap();
        drop(writer);

        let mut file = std::fs::File::open(&path).unwrap();
        let mut magic = [0u8; 8];
        file.read_exact(&mut magic).unwrap();
        assert_eq!(&magic, b"TOKIOTRC");

        let mut version = [0u8; 4];
        file.read_exact(&mut version).unwrap();
        assert_eq!(u32::from_le_bytes(version), format::VERSION);
    }

    #[test]
    fn test_rotating_writer_creation() {
        let dir = TempDir::new().unwrap();
        let base = dir.path().join("trace");
        let mut writer = RotatingWriter::new(&base, 1024, 4096).unwrap();
        writer.seal().unwrap();

        let events = read_trace_events(&rotating_file(&base, 0));
        assert_eq!(events.len(), 0);
    }

    #[test]
    fn test_rotating_writer_rotation() {
        let dir = TempDir::new().unwrap();
        let base = dir.path().join("trace");
        let event_size = 11u64;
        let max_file_size = format::HEADER_SIZE as u64 + event_size * 2;
        let mut writer = RotatingWriter::new(&base, max_file_size, 10000).unwrap();

        for _ in 0..3 {
            writer.write_event(&park_event()).unwrap();
        }
        writer.seal().unwrap();

        // First file should have 2 events, second file should have 1
        let e0 = read_trace_events(&rotating_file(&base, 0));
        let e1 = read_trace_events(&rotating_file(&base, 1));
        assert_eq!(e0.len() + e1.len(), 3);
    }

    #[test]
    fn test_rotating_writer_eviction() {
        let dir = TempDir::new().unwrap();
        let base = dir.path().join("trace");
        let event_size = 11u64;
        let header = format::HEADER_SIZE as u64;
        let max_file_size = header + event_size * 2;
        let max_total_size = max_file_size * 3;
        let mut writer = RotatingWriter::new(&base, max_file_size, max_total_size).unwrap();

        for _ in 0..10 {
            writer.write_event(&park_event()).unwrap();
        }
        writer.seal().unwrap();

        // Key invariant: total disk usage stays within budget
        assert!(total_disk_usage(dir.path()) <= max_total_size);

        // Oldest files should be evicted
        assert!(!std::path::Path::new(&rotating_file(&base, 0)).exists());

        // Surviving files should all be readable
        for i in 2..=4 {
            let f = rotating_file(&base, i);
            if std::path::Path::new(&f).exists() {
                read_trace_events(&f); // panics if corrupt
            }
        }
    }

    #[test]
    fn test_rotating_writer_stops_when_over_budget() {
        let dir = TempDir::new().unwrap();
        let base = dir.path().join("trace");
        let event_size = 11u64;
        let header = format::HEADER_SIZE as u64;
        let max_file_size = header + event_size * 10;
        let max_total_size = header + event_size * 2;
        let mut writer = RotatingWriter::new(&base, max_file_size, max_total_size).unwrap();

        for _ in 0..100 {
            writer.write_event(&park_event()).unwrap();
        }
        writer.seal().unwrap();

        // Disk usage bounded: at most one event over budget (the one that triggered stop)
        let usage = total_disk_usage(dir.path());
        assert!(
            usage <= max_total_size + event_size,
            "disk usage {} exceeds budget {} + one event {}",
            usage,
            max_total_size,
            event_size
        );
        // File must be readable, not corrupted
        let events = read_trace_events(&rotating_file(&base, 0));
        assert!(events.len() < 100, "should have stopped writing");
    }

    /// Bug: write_event_inner sets stopped=true when total_size slightly exceeds
    /// max_total_size, without attempting eviction. This happens right after
    /// rotate() + evict_oldest() brings total_size just under budget, then the
    /// first event in the new file pushes it a few bytes over. The writer
    /// permanently stops even though eviction could free space.
    ///
    /// Reproduces the stress test failure: 64-worker runtime with 1MB segments
    /// and 100MB budget stops producing segments after ~100 rotations.
    #[test]
    fn test_writer_stops_on_tiny_overshoot_after_eviction() {
        let dir = TempDir::new().unwrap();
        let base = dir.path().join("trace");
        let _header = format::HEADER_SIZE as u64;
        // Use max_file_size that doesn't evenly divide by event size,
        // so files end up slightly under max_file_size (with leftover bytes).
        // Over 100 files, these leftovers accumulate and push total_size
        // past max_total_size after eviction.
        let max_file_size = 200;
        let num_files = 100u64;
        let max_total_size = max_file_size * num_files;
        let mut writer = RotatingWriter::new(&base, max_file_size, max_total_size).unwrap();

        // Write many events. The event size (11 bytes for park_event) doesn't
        // divide evenly into (max_file_size - header), so each file wastes a
        // few bytes. After 100 rotations, total_size drifts above max_total_size.
        for i in 0..5000 {
            writer.write_event(&park_event()).unwrap();
            if writer.stopped {
                panic!(
                    "Writer stopped at event {i}! total_size={}, max_total_size={}, \
                     current_size={}, files={}. \
                     write_event_inner should try eviction before stopping.",
                    writer.total_size,
                    max_total_size,
                    writer.current_size,
                    writer.files.len()
                );
            }
        }
    }

    #[test]
    fn test_rotating_writer_file_naming() {
        let dir = TempDir::new().unwrap();
        let base = dir.path().join("trace");
        let event_size = 11u64;
        let max_file_size = format::HEADER_SIZE as u64 + event_size;
        let mut writer = RotatingWriter::new(&base, max_file_size, 100000).unwrap();

        for _ in 0..5 {
            writer.write_event(&park_event()).unwrap();
        }
        writer.seal().unwrap();

        // With rotate-after-overflow, each file gets 2 events (1 fits + 1 overflows).
        // 5 events → 3 files (2+2+1).
        for i in 0..3 {
            let file = rotating_file(&base, i);
            assert!(
                std::path::Path::new(&file).exists(),
                "File {} should exist",
                file
            );
        }
    }

    #[test]
    fn test_write_batch_across_rotation_boundary() {
        let dir = TempDir::new().unwrap();
        let base = dir.path().join("trace");
        let event_size = 11u64;
        let header = format::HEADER_SIZE as u64;
        let max_file_size = header + event_size;
        let mut writer = RotatingWriter::new(&base, max_file_size, 100000).unwrap();

        let events: Vec<_> = (0..3).map(|_| park_event()).collect();
        writer.write_atomic(&events).unwrap();
        writer.seal().unwrap();

        // All 3 events should be readable across the rotated files.
        // With rotate-after-overflow: file 0 gets 2 events, file 1 gets 1.
        let total: usize = (0..2)
            .map(|i| read_trace_events(&rotating_file(&base, i)).len())
            .sum();
        assert_eq!(total, 3);
    }

    #[test]
    fn test_rotated_files_have_valid_headers() {
        let dir = TempDir::new().unwrap();
        let base = dir.path().join("trace");
        let event_size = 11u64;
        let header = format::HEADER_SIZE as u64;
        // With rotate-after-overflow, the file that exceeds the limit keeps
        // the overflowing event. So header + 1 event = exactly at limit,
        // the 2nd event overflows and stays, then rotation happens.
        let max_file_size = header + event_size;
        let mut writer = RotatingWriter::new(&base, max_file_size, 100000).unwrap();

        for _ in 0..3 {
            writer.write_event(&park_event()).unwrap();
        }
        writer.seal().unwrap();

        // Each rotated file must be a self-contained, readable trace.
        // With rotate-after-overflow, first file gets 2 events (1 fits + 1 overflows),
        // second file gets 1 event.
        let total: usize = (0..2)
            .map(|i| read_trace_events(&rotating_file(&base, i)).len())
            .sum();
        assert_eq!(total, 3);
    }

    #[test]
    fn test_flush_after_stop() {
        let dir = TempDir::new().unwrap();
        let base = dir.path().join("trace");
        let event_size = 11u64;
        let header = format::HEADER_SIZE as u64;
        let max_total_size = header + event_size;
        let mut writer = RotatingWriter::new(&base, 10000, max_total_size).unwrap();

        for _ in 0..5 {
            writer.write_event(&park_event()).unwrap();
        }
        // Repeated flush after stop should not error
        assert!(writer.flush().is_ok());
        assert!(writer.flush().is_ok());
    }

    #[test]
    fn test_mixed_event_sizes() {
        let dir = TempDir::new().unwrap();
        let base = dir.path().join("trace");
        let header = format::HEADER_SIZE as u64;
        let max_file_size = header + 15;
        let mut writer = RotatingWriter::new(&base, max_file_size, 100000).unwrap();

        // WorkerPark = 11 bytes, WorkerUnpark = 15 bytes
        let events = [
            RawEvent::WorkerPark {
                timestamp_nanos: 1000,
                worker_id: 0,
                worker_local_queue_depth: 2,
                cpu_time_nanos: 0,
            },
            RawEvent::WorkerPark {
                timestamp_nanos: 2000,
                worker_id: 1,
                worker_local_queue_depth: 0,
                cpu_time_nanos: 0,
            },
            RawEvent::WorkerUnpark {
                timestamp_nanos: 3000,
                worker_id: 0,
                worker_local_queue_depth: 2,
                cpu_time_nanos: 0,
                sched_wait_delta_nanos: 0,
            },
        ];
        for e in &events {
            writer.write_event(e).unwrap();
        }
        writer.seal().unwrap();

        // All events should be readable across files.
        // With rotate-after-overflow, files can slightly exceed max_file_size.
        let mut total = 0;
        for i in 0..10 {
            let f = rotating_file(&base, i);
            if std::path::Path::new(&f).exists() {
                total += read_trace_events(&f).len();
            }
        }
        assert_eq!(total, 3);
    }

    #[test]
    fn test_max_file_size_smaller_than_header() {
        let dir = TempDir::new().unwrap();
        let base = dir.path().join("trace");
        // max_file_size smaller than header — every event triggers rotation
        let mut writer = RotatingWriter::new(&base, 5, 100000).unwrap();

        for _ in 0..3 {
            writer.write_event(&park_event()).unwrap();
        }
        writer.seal().unwrap();

        // Should not panic/infinite-loop, and all events should be readable somewhere
        let total: usize = (0..10)
            .map(|i| {
                let f = rotating_file(&base, i);
                if std::path::Path::new(&f).exists() {
                    read_trace_events(&f).len()
                } else {
                    0
                }
            })
            .sum();
        assert_eq!(total, 3);
    }

    #[test]
    fn test_event_exactly_on_max_file_size_boundary() {
        let dir = TempDir::new().unwrap();
        let base = dir.path().join("trace");
        let event_size = 11u64;
        let header = format::HEADER_SIZE as u64;
        // Exactly fits header + 1 event
        let max_file_size = header + event_size;
        let mut writer = RotatingWriter::new(&base, max_file_size, 100000).unwrap();

        for _ in 0..2 {
            writer.write_event(&park_event()).unwrap();
        }
        writer.seal().unwrap();

        // Both events readable. With rotate-after-overflow, the first event
        // fits exactly (no overflow), so the second event overflows and stays
        // in the same file, then rotation happens.
        let e0 = read_trace_events(&rotating_file(&base, 0));
        assert_eq!(e0.len(), 2);
    }

    #[test]
    fn test_active_suffix_while_writing() {
        let dir = TempDir::new().unwrap();
        let base = dir.path().join("trace");
        let mut writer = RotatingWriter::new(&base, 1024, 100000).unwrap();
        writer.write_event(&park_event()).unwrap();
        writer.flush().unwrap();

        // Current file should have .active suffix
        let active = dir.path().join("trace.0.bin.active");
        assert!(active.exists(), "active file should exist while writing");
        let sealed = dir.path().join("trace.0.bin");
        assert!(!sealed.exists(), "sealed file should not exist yet");
    }

    #[test]
    fn test_rotation_seals_previous_file() {
        let dir = TempDir::new().unwrap();
        let base = dir.path().join("trace");
        let event_size = 11u64;
        let max_file_size = format::HEADER_SIZE as u64 + event_size;
        let mut writer = RotatingWriter::new(&base, max_file_size, 100000).unwrap();

        // Write 2 events — triggers rotation after first
        writer.write_event(&park_event()).unwrap();
        writer.write_event(&park_event()).unwrap();
        writer.flush().unwrap();

        // First file should be sealed (.bin), second should be active
        assert!(
            dir.path().join("trace.0.bin").exists(),
            "rotated file should be sealed"
        );
        assert!(
            !dir.path().join("trace.0.bin.active").exists(),
            "rotated file should not be active"
        );
        assert!(
            dir.path().join("trace.1.bin.active").exists(),
            "current file should be active"
        );
        assert!(
            !dir.path().join("trace.1.bin").exists(),
            "current file should not be sealed"
        );
    }

    #[test]
    fn test_seal_renames_current_file() {
        let dir = TempDir::new().unwrap();
        let base = dir.path().join("trace");
        let mut writer = RotatingWriter::new(&base, 1024, 100000).unwrap();
        writer.write_event(&park_event()).unwrap();
        writer.seal().unwrap();

        assert!(
            dir.path().join("trace.0.bin").exists(),
            "file should be sealed after seal()"
        );
        assert!(
            !dir.path().join("trace.0.bin.active").exists(),
            "active file should be gone after seal()"
        );
    }

    #[test]
    fn test_single_file_no_active_suffix() {
        let dir = TempDir::new().unwrap();
        let path = dir.path().join("test.bin");
        let mut writer = RotatingWriter::single_file(&path).unwrap();
        writer.write_event(&park_event()).unwrap();
        writer.flush().unwrap();

        // single_file writes directly to the given path, no .active suffix
        assert!(path.exists());
        assert!(!dir.path().join("test.bin.active").exists());
    }

    #[test]
    fn test_segment_metadata_roundtrip() {
        let dir = TempDir::new().unwrap();
        let path = dir.path().join("trace.bin");
        let mut writer = RotatingWriter::single_file(&path).unwrap();
        writer
            .set_segment_metadata(vec![
                ("service".into(), "checkout-api".into()),
                ("host".into(), "i-0abc123".into()),
            ])
            .unwrap();
        writer.write_event(&park_event()).unwrap();
        writer.flush().unwrap();

        // TraceReader absorbs SegmentMetadata into its segment_metadata field
        let mut reader = TraceReader::new(path.to_str().unwrap()).unwrap();
        reader.read_header().unwrap();
        let events = reader.read_all().unwrap();
        assert_eq!(events.len(), 1); // only the park event
        assert_eq!(reader.segment_metadata.len(), 2);
        assert_eq!(
            reader.segment_metadata[0],
            ("service".to_string(), "checkout-api".to_string())
        );
        assert_eq!(
            reader.segment_metadata[1],
            ("host".to_string(), "i-0abc123".to_string())
        );
    }

    #[test]
    fn test_segment_metadata_written_in_every_rotated_file() {
        let dir = TempDir::new().unwrap();
        let base = dir.path().join("trace");
        let event_size = 11u64; // WorkerPark
        let metadata_size = format::wire_event_size(&TelemetryEvent::SegmentMetadata {
            timestamp_nanos: 0,
            entries: vec![("k".into(), "v".into())],
        }) as u64;
        // File fits header + metadata + 1 event, rotation on 2nd event
        let max_file_size = format::HEADER_SIZE as u64 + metadata_size + event_size;
        let mut writer = RotatingWriter::new(&base, max_file_size, 100_000).unwrap();
        writer
            .set_segment_metadata(vec![("k".into(), "v".into())])
            .unwrap();

        // Write 3 events
        for _ in 0..3 {
            writer.write_event(&park_event()).unwrap();
        }
        writer.flush().unwrap();
        writer.seal().unwrap();

        // Check each file has SegmentMetadata
        let mut files: Vec<_> = std::fs::read_dir(dir.path())
            .unwrap()
            .filter_map(|e| e.ok())
            .map(|e| e.path())
            .filter(|p| p.extension().is_some_and(|ext| ext == "bin"))
            .collect();
        files.sort();
        assert!(files.len() >= 2, "expected at least 2 files from rotation");

        for file in &files {
            let path = file.to_str().unwrap();
            let mut reader = TraceReader::new(path).unwrap();
            reader.read_header().unwrap();
            let _events = reader.read_all().unwrap();
            assert_eq!(
                reader.segment_metadata,
                vec![("k".to_string(), "v".to_string())],
                "{}: expected SegmentMetadata",
                path
            );
        }
    }

    #[test]
    fn test_segment_metadata_empty_entries() {
        let dir = TempDir::new().unwrap();
        let path = dir.path().join("trace.bin");
        let mut writer = RotatingWriter::single_file(&path).unwrap();
        writer.set_segment_metadata(vec![]).unwrap();
        writer.write_event(&park_event()).unwrap();
        writer.flush().unwrap();

        let mut reader = TraceReader::new(path.to_str().unwrap()).unwrap();
        reader.read_header().unwrap();
        let events = reader.read_all().unwrap();
        assert_eq!(events.len(), 1);
        assert!(reader.segment_metadata.is_empty());
    }
}
