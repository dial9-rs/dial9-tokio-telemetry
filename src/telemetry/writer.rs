use crate::telemetry::events::TelemetryEvent;
use crate::telemetry::format;
use std::collections::VecDeque;
use std::fs::{self, File};
use std::io::{BufWriter, Write};
use std::path::{Path, PathBuf};

pub trait TraceWriter: Send {
    fn write_event(&mut self, event: &TelemetryEvent) -> std::io::Result<()>;
    fn write_batch(&mut self, events: &[TelemetryEvent]) -> std::io::Result<()>;
    fn flush(&mut self) -> std::io::Result<()>;
    /// Returns true if the writer rotated to a new file since the last call to this method.
    /// Used by the flush path to know when to re-emit SpawnLocationDefs.
    fn take_rotated(&mut self) -> bool {
        false
    }
}

pub struct SimpleBinaryWriter {
    writer: BufWriter<File>,
}

impl SimpleBinaryWriter {
    pub fn new(path: impl AsRef<Path>) -> std::io::Result<Self> {
        let file = File::create(path)?;
        let mut writer = BufWriter::new(file);
        format::write_header(&mut writer)?;
        Ok(Self { writer })
    }
}

impl TraceWriter for SimpleBinaryWriter {
    fn write_event(&mut self, event: &TelemetryEvent) -> std::io::Result<()> {
        format::write_event(&mut self.writer, event)
    }

    fn write_batch(&mut self, events: &[TelemetryEvent]) -> std::io::Result<()> {
        for event in events {
            format::write_event(&mut self.writer, event)?;
        }
        Ok(())
    }

    fn flush(&mut self) -> std::io::Result<()> {
        self.writer.flush()
    }
}

/// A writer that discards all events. Useful for benchmarking hook overhead
/// without I/O costs.
pub struct NullWriter;

impl TraceWriter for NullWriter {
    fn write_event(&mut self, _event: &TelemetryEvent) -> std::io::Result<()> {
        Ok(())
    }
    fn write_batch(&mut self, _events: &[TelemetryEvent]) -> std::io::Result<()> {
        Ok(())
    }
    fn flush(&mut self) -> std::io::Result<()> {
        Ok(())
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
        let first_path = Self::file_path(&base_path, 0);
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
        })
    }

    fn file_path(base: &Path, index: u32) -> PathBuf {
        let stem = base.file_stem().unwrap_or_default().to_string_lossy();
        let parent = base.parent().unwrap_or(Path::new("."));
        parent.join(format!("{}.{}.bin", stem, index))
    }

    fn rotate(&mut self) -> std::io::Result<()> {
        self.current_writer.flush()?;
        // Update the size of the file we're closing.
        if let Some(last) = self.files.back_mut() {
            last.1 = self.current_size;
        }

        let new_path = Self::file_path(&self.base_path, self.next_index);
        self.next_index += 1;
        let file = File::create(&new_path)?;
        self.current_writer = BufWriter::new(file);
        format::write_header(&mut self.current_writer)?;
        let header_size = format::HEADER_SIZE as u64;
        self.current_size = header_size;
        self.total_size += header_size;
        self.files.push_back((new_path, header_size));
        self.rotated = true;

        self.evict_oldest()?;
        Ok(())
    }

    fn evict_oldest(&mut self) -> std::io::Result<()> {
        // Always keep at least the current file.
        while self.total_size > self.max_total_size && self.files.len() > 1 {
            if let Some((path, size)) = self.files.pop_front() {
                self.total_size -= size;
                let _ = fs::remove_file(&path);
            }
        }
        // If even the current file alone exceeds total budget, stop writing.
        if self.total_size > self.max_total_size {
            self.stopped = true;
        }
        Ok(())
    }

    fn write_event_inner(&mut self, event: &TelemetryEvent) -> std::io::Result<()> {
        if self.stopped {
            return Ok(());
        }
        let event_size = format::wire_event_size(event) as u64;
        if self.current_size + event_size > self.max_file_size {
            self.rotate()?;
            if self.stopped {
                return Ok(());
            }
        }
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
            self.stopped = true;
        }
        Ok(())
    }
}

impl TraceWriter for RotatingWriter {
    fn write_event(&mut self, event: &TelemetryEvent) -> std::io::Result<()> {
        self.write_event_inner(event)
    }

    fn write_batch(&mut self, events: &[TelemetryEvent]) -> std::io::Result<()> {
        for event in events {
            self.write_event_inner(event)?;
        }
        Ok(())
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
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::telemetry::analysis::TraceReader;
    use crate::telemetry::format;
    use crate::telemetry::task_metadata::{UNKNOWN_SPAWN_LOCATION_ID, UNKNOWN_TASK_ID};
    use std::io::Read;
    use tempfile::TempDir;

    fn park_event() -> TelemetryEvent {
        TelemetryEvent::WorkerPark {
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

    /// Total size of all .bin files in a directory.
    fn total_disk_usage(dir: &std::path::Path) -> u64 {
        std::fs::read_dir(dir)
            .unwrap()
            .filter_map(|e| e.ok())
            .filter(|e| e.path().extension().map_or(false, |ext| ext == "bin"))
            .map(|e| e.metadata().unwrap().len())
            .sum()
    }

    #[test]
    fn test_writer_creation() {
        let dir = TempDir::new().unwrap();
        let path = dir.path().join("test_trace_v2.bin");
        let writer = SimpleBinaryWriter::new(&path);
        assert!(writer.is_ok());
    }

    #[test]
    fn test_write_event() {
        let dir = TempDir::new().unwrap();
        let path = dir.path().join("test_event_v2.bin");
        let mut writer = SimpleBinaryWriter::new(&path).unwrap();

        let event = park_event();
        assert!(writer.write_event(&event).is_ok());
        assert!(writer.flush().is_ok());

        let metadata = std::fs::metadata(&path).unwrap();
        let expected = format::HEADER_SIZE + format::wire_event_size(&event);
        assert_eq!(metadata.len(), expected as u64);
    }

    #[test]
    fn test_write_batch_sizes() {
        let dir = TempDir::new().unwrap();
        let path = dir.path().join("test_batch_v2.bin");
        let mut writer = SimpleBinaryWriter::new(&path).unwrap();

        let events = vec![
            TelemetryEvent::PollStart {
                timestamp_nanos: 1000,
                worker_id: 0,
                worker_local_queue_depth: 2,
                task_id: UNKNOWN_TASK_ID,
                spawn_loc_id: UNKNOWN_SPAWN_LOCATION_ID,
            }, // 13 bytes
            TelemetryEvent::WorkerPark {
                timestamp_nanos: 1000,
                worker_id: 0,
                worker_local_queue_depth: 2,
                cpu_time_nanos: 0,
            }, // 11 bytes
        ];

        writer.write_batch(&events).unwrap();
        writer.flush().unwrap();

        let metadata = std::fs::metadata(&path).unwrap();
        assert_eq!(metadata.len(), (format::HEADER_SIZE + 13 + 11) as u64); // 12 + 13 + 11 = 36
    }

    #[test]
    fn test_binary_format_header() {
        let dir = TempDir::new().unwrap();
        let path = dir.path().join("test_format_v2.bin");
        let writer = SimpleBinaryWriter::new(&path).unwrap();
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
        let writer = RotatingWriter::new(&base, 1024, 4096).unwrap();
        drop(writer);

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
        writer.flush().unwrap();

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
        writer.flush().unwrap();

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
        writer.flush().unwrap();

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
        writer.flush().unwrap();

        for i in 0..5 {
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
        writer.write_batch(&events).unwrap();
        writer.flush().unwrap();

        // All 3 events should be readable across the rotated files
        let total: usize = (0..3)
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
        let max_file_size = header + event_size;
        let mut writer = RotatingWriter::new(&base, max_file_size, 100000).unwrap();

        for _ in 0..3 {
            writer.write_event(&park_event()).unwrap();
        }
        writer.flush().unwrap();

        // Each rotated file must be a self-contained, readable trace
        for i in 0..3 {
            let events = read_trace_events(&rotating_file(&base, i));
            assert_eq!(events.len(), 1, "file {} should have exactly 1 event", i);
        }
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

        let events = [
            TelemetryEvent::WorkerPark {
                timestamp_nanos: 1000,
                worker_id: 0,
                worker_local_queue_depth: 2,
                cpu_time_nanos: 0,
            }, // 11 bytes
            TelemetryEvent::PollStart {
                timestamp_nanos: 1000,
                worker_id: 0,
                worker_local_queue_depth: 2,
                task_id: UNKNOWN_TASK_ID,
                spawn_loc_id: UNKNOWN_SPAWN_LOCATION_ID,
            }, // 13 bytes
            TelemetryEvent::WorkerUnpark {
                timestamp_nanos: 1000,
                worker_id: 0,
                worker_local_queue_depth: 2,
                cpu_time_nanos: 0,
                sched_wait_delta_nanos: 0,
            }, // 15 bytes
        ];
        for e in &events {
            writer.write_event(e).unwrap();
        }
        writer.flush().unwrap();

        // All events should be readable, one per file
        for i in 0..3 {
            let read = read_trace_events(&rotating_file(&base, i));
            assert_eq!(read.len(), 1, "file {} should have 1 event", i);
        }
        // No file should exceed max_file_size
        for i in 0..3 {
            let size = std::fs::metadata(rotating_file(&base, i)).unwrap().len();
            assert!(
                size <= max_file_size,
                "file {} is {} > max {}",
                i,
                size,
                max_file_size
            );
        }
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
        writer.flush().unwrap();

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
        writer.flush().unwrap();

        // Both events readable, no file exceeds max_file_size
        let e0 = read_trace_events(&rotating_file(&base, 0));
        let e1 = read_trace_events(&rotating_file(&base, 1));
        assert_eq!(e0.len() + e1.len(), 2);
        assert!(std::fs::metadata(rotating_file(&base, 0)).unwrap().len() <= max_file_size);
        assert!(std::fs::metadata(rotating_file(&base, 1)).unwrap().len() <= max_file_size);
    }
}
