use crate::telemetry::events::TelemetryEvent;
use crate::telemetry::format;
use dial9_trace_format::encoder::Encoder;
use std::collections::VecDeque;
use std::fs::{self, File};
use std::io::{self, BufWriter, Write};
use std::path::{Path, PathBuf};

/// Result of a [`TraceWriter::write_atomic`] call.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum WriteAtomicResult {
    /// All events were written to the current file.
    Written,
    /// A file rotation occurred before writing. The events were not written;
    /// callers should re-emit defs and retry into the new file.
    Rotated,
    /// The batch is larger than `max_file_size` and will never fit in a single
    /// file. The events were not written. Callers should skip this batch rather
    /// than retrying in an infinite loop.
    OversizedBatch,
}

pub trait TraceWriter: Send {
    fn write_event(&mut self, event: &TelemetryEvent) -> std::io::Result<()>;
    fn write_batch(&mut self, events: &[TelemetryEvent]) -> std::io::Result<()>;
    fn flush(&mut self) -> std::io::Result<()>;
    /// Returns true if the writer rotated to a new file since the last call to this method.
    /// Used by the flush path to know when to re-emit SpawnLocationDefs.
    fn take_rotated(&mut self) -> bool {
        false
    }
    /// Write a group of events atomically — all events land in the same file.
    /// Rotating writers pre-check total size and rotate before writing if needed.
    fn write_atomic(&mut self, events: &[TelemetryEvent]) -> std::io::Result<WriteAtomicResult> {
        for event in events {
            self.write_event(event)?;
        }
        Ok(WriteAtomicResult::Written)
    }
}

impl<W: TraceWriter + ?Sized> TraceWriter for Box<W> {
    fn write_event(&mut self, event: &TelemetryEvent) -> std::io::Result<()> {
        (**self).write_event(event)
    }
    fn write_batch(&mut self, events: &[TelemetryEvent]) -> std::io::Result<()> {
        (**self).write_batch(events)
    }
    fn flush(&mut self) -> std::io::Result<()> {
        (**self).flush()
    }
    fn take_rotated(&mut self) -> bool {
        (**self).take_rotated()
    }
    fn write_atomic(&mut self, events: &[TelemetryEvent]) -> std::io::Result<WriteAtomicResult> {
        (**self).write_atomic(events)
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

/// A [`Write`] wrapper that counts bytes written.
struct CountingWriter<W> {
    inner: W,
    count: u64,
}

impl<W: Write> CountingWriter<W> {
    fn new(inner: W) -> Self {
        Self { inner, count: 0 }
    }
}

impl<W: Write> Write for CountingWriter<W> {
    fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
        let n = self.inner.write(buf)?;
        self.count += n as u64;
        Ok(n)
    }
    fn flush(&mut self) -> io::Result<()> {
        self.inner.flush()
    }
}

/// A writer that rotates trace files to bound disk usage.
///
/// - `max_file_size`: rotate to a new file when the current file exceeds this size
/// - `max_total_size`: delete oldest files when total size across all files exceeds this
///
/// Files are named `{base_path}.0.bin`, `{base_path}.1.bin`, etc.
/// Each file is a self-contained trace with its own header and schemas.
pub struct RotatingWriter {
    base_path: PathBuf,
    max_file_size: u64,
    max_total_size: u64,
    /// Tracks (path, size) of files oldest-first.
    files: VecDeque<(PathBuf, u64)>,
    total_size: u64,
    encoder: Encoder<CountingWriter<BufWriter<File>>>,
    next_index: u32,
    /// Set when we've hit the total size cap; silently drops further events.
    stopped: bool,
    /// Set to true when a rotation occurs; cleared by `take_rotated`.
    rotated: bool,
    /// True once at least one event has been written to the current file.
    has_events_in_current_file: bool,
}

impl RotatingWriter {
    fn open_encoder(path: &Path) -> io::Result<(Encoder<CountingWriter<BufWriter<File>>>, u64)> {
        let file = File::create(path)?;
        let counting = CountingWriter::new(BufWriter::new(file));
        let mut encoder = Encoder::new_to(counting)?;
        format::register_schemas(&mut encoder)?;
        // Flush to ensure all preamble bytes are counted
        encoder.flush()?;
        let preamble_size = encoder.as_inner().count;
        Ok((encoder, preamble_size))
    }

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
        let (encoder, preamble_size) = Self::open_encoder(&first_path)?;

        let mut files = VecDeque::new();
        files.push_back((first_path, preamble_size));

        Ok(Self {
            base_path,
            max_file_size,
            max_total_size,
            files,
            total_size: preamble_size,
            encoder,
            next_index: 1,
            stopped: false,
            rotated: false,
            has_events_in_current_file: false,
        })
    }

    /// Create a writer that writes to a single file with no rotation or eviction.
    pub fn single_file(path: impl Into<PathBuf>) -> std::io::Result<Self> {
        let path = path.into();
        if let Some(parent) = path.parent() {
            fs::create_dir_all(parent)?;
        }
        let (encoder, preamble_size) = Self::open_encoder(&path)?;

        let mut files = VecDeque::new();
        files.push_back((path.clone(), preamble_size));

        Ok(Self {
            base_path: path,
            max_file_size: u64::MAX,
            max_total_size: u64::MAX,
            files,
            total_size: preamble_size,
            encoder,
            next_index: 1,
            stopped: false,
            rotated: false,
            has_events_in_current_file: false,
        })
    }

    fn file_path(base: &Path, index: u32) -> PathBuf {
        let stem = base.file_stem().unwrap_or_default().to_string_lossy();
        let parent = base.parent().unwrap_or(Path::new("."));
        parent.join(format!("{}.{}.bin", stem, index))
    }

    fn bytes_written(&self) -> u64 {
        self.encoder.as_inner().count
    }

    fn rotate(&mut self) -> std::io::Result<()> {
        self.encoder.flush()?;
        // Update the size of the file we're closing.
        let closing_size = self.bytes_written();
        if let Some(last) = self.files.back_mut() {
            last.1 = closing_size;
        }
        // Recompute total_size from tracked files
        self.total_size = self.files.iter().map(|(_, s)| s).sum();

        let new_path = Self::file_path(&self.base_path, self.next_index);
        self.next_index += 1;
        let (encoder, preamble_size) = Self::open_encoder(&new_path)?;
        self.encoder = encoder;
        self.total_size += preamble_size;
        self.files.push_back((new_path, preamble_size));
        self.rotated = true;
        self.has_events_in_current_file = false;

        self.evict_oldest()?;
        Ok(())
    }

    fn evict_oldest(&mut self) -> std::io::Result<()> {
        while self.total_size > self.max_total_size && self.files.len() > 1 {
            if let Some((path, size)) = self.files.pop_front() {
                self.total_size -= size;
                let _ = fs::remove_file(&path);
            }
        }
        if self.total_size > self.max_total_size {
            self.stopped = true;
        }
        Ok(())
    }

    fn maybe_rotate(&mut self) -> std::io::Result<()> {
        if self.stopped {
            return Ok(());
        }
        // Don't rotate a file that only has the preamble — it would loop forever.
        if self.bytes_written() >= self.max_file_size && self.has_events_in_current_file {
            self.rotate()?;
        }
        Ok(())
    }

    fn write_event_inner(&mut self, event: &TelemetryEvent) -> std::io::Result<()> {
        if self.stopped {
            return Ok(());
        }
        self.maybe_rotate()?;
        if self.stopped {
            return Ok(());
        }
        format::write_event(&mut self.encoder, event)?;
        self.has_events_in_current_file = true;
        // Check total budget
        let current = self.bytes_written();
        let prev_total: u64 = self.files.iter().rev().skip(1).map(|(_, s)| s).sum();
        if prev_total + current > self.max_total_size {
            self.encoder.flush()?;
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
            self.encoder.flush()?;
        }
        Ok(())
    }

    fn take_rotated(&mut self) -> bool {
        std::mem::replace(&mut self.rotated, false)
    }

    fn write_atomic(&mut self, events: &[TelemetryEvent]) -> std::io::Result<WriteAtomicResult> {
        if self.stopped {
            return Ok(WriteAtomicResult::Written);
        }
        // If we're already past the file size limit, rotate first.
        self.maybe_rotate()?;
        if self.rotated {
            return Ok(WriteAtomicResult::Rotated);
        }
        for event in events {
            format::write_event(&mut self.encoder, event)?;
        }
        if !events.is_empty() {
            self.has_events_in_current_file = true;
        }
        Ok(WriteAtomicResult::Written)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::telemetry::analysis::TraceReader;
    use crate::telemetry::task_metadata::{UNKNOWN_SPAWN_LOCATION_ID, UNKNOWN_TASK_ID};
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

    /// Read all events from a trace file.
    fn read_trace_events(path: &str) -> Vec<TelemetryEvent> {
        let mut reader = TraceReader::new(path).unwrap();
        reader.read_all().unwrap()
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

        let event = park_event();
        assert!(writer.write_event(&event).is_ok());
        assert!(writer.flush().is_ok());

        let events = read_trace_events(path.to_str().unwrap());
        assert_eq!(events.len(), 1);
    }

    #[test]
    fn test_write_batch_roundtrip() {
        let dir = TempDir::new().unwrap();
        let path = dir.path().join("test_batch_v2.bin");
        let mut writer = RotatingWriter::single_file(&path).unwrap();

        let events = vec![
            TelemetryEvent::PollStart {
                timestamp_nanos: 1000,
                worker_id: 0,
                worker_local_queue_depth: 2,
                task_id: UNKNOWN_TASK_ID,
                spawn_loc_id: UNKNOWN_SPAWN_LOCATION_ID,
            },
            TelemetryEvent::WorkerPark {
                timestamp_nanos: 1000,
                worker_id: 0,
                worker_local_queue_depth: 2,
                cpu_time_nanos: 0,
            },
        ];

        writer.write_batch(&events).unwrap();
        writer.flush().unwrap();

        let decoded = read_trace_events(path.to_str().unwrap());
        assert_eq!(decoded.len(), 2);
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
        // Preamble is ~300 bytes; set max to preamble + room for ~1 event
        let mut writer = RotatingWriter::new(&base, 350, 100000).unwrap();

        for _ in 0..3 {
            writer.write_event(&park_event()).unwrap();
        }
        writer.flush().unwrap();

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
    fn test_rotating_writer_eviction() {
        let dir = TempDir::new().unwrap();
        let base = dir.path().join("trace");
        let mut writer = RotatingWriter::new(&base, 350, 1200).unwrap();

        for _ in 0..10 {
            writer.write_event(&park_event()).unwrap();
        }
        writer.flush().unwrap();

        // Oldest files should be evicted
        assert!(!std::path::Path::new(&rotating_file(&base, 0)).exists());
    }

    #[test]
    fn test_rotating_writer_stops_when_over_budget() {
        let dir = TempDir::new().unwrap();
        let base = dir.path().join("trace");
        // total budget barely fits the preamble + a few events
        let mut writer = RotatingWriter::new(&base, 10000, 400).unwrap();

        for _ in 0..100 {
            writer.write_event(&park_event()).unwrap();
        }
        writer.flush().unwrap();

        let events = read_trace_events(&rotating_file(&base, 0));
        assert!(events.len() < 100, "should have stopped writing");
    }

    #[test]
    fn test_rotating_writer_file_naming() {
        let dir = TempDir::new().unwrap();
        let base = dir.path().join("trace");
        let mut writer = RotatingWriter::new(&base, 330, 100000).unwrap();

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
    fn test_rotated_files_have_valid_headers() {
        let dir = TempDir::new().unwrap();
        let base = dir.path().join("trace");
        let mut writer = RotatingWriter::new(&base, 350, 100000).unwrap();

        for _ in 0..3 {
            writer.write_event(&park_event()).unwrap();
        }
        writer.flush().unwrap();

        for i in 0..10 {
            let f = rotating_file(&base, i);
            if std::path::Path::new(&f).exists() {
                // Must be readable — panics if corrupt
                read_trace_events(&f);
            }
        }
    }

    #[test]
    fn test_flush_after_stop() {
        let dir = TempDir::new().unwrap();
        let base = dir.path().join("trace");
        let mut writer = RotatingWriter::new(&base, 10000, 400).unwrap();

        for _ in 0..5 {
            writer.write_event(&park_event()).unwrap();
        }
        assert!(writer.flush().is_ok());
        assert!(writer.flush().is_ok());
    }

    #[test]
    fn test_mixed_event_sizes() {
        let dir = TempDir::new().unwrap();
        let base = dir.path().join("trace");
        let mut writer = RotatingWriter::new(&base, 360, 100000).unwrap();

        let events = [
            TelemetryEvent::WorkerPark {
                timestamp_nanos: 1000,
                worker_id: 0,
                worker_local_queue_depth: 2,
                cpu_time_nanos: 0,
            },
            TelemetryEvent::PollStart {
                timestamp_nanos: 1000,
                worker_id: 0,
                worker_local_queue_depth: 2,
                task_id: UNKNOWN_TASK_ID,
                spawn_loc_id: UNKNOWN_SPAWN_LOCATION_ID,
            },
            TelemetryEvent::WorkerUnpark {
                timestamp_nanos: 1000,
                worker_id: 0,
                worker_local_queue_depth: 2,
                cpu_time_nanos: 0,
                sched_wait_delta_nanos: 0,
            },
        ];
        for e in &events {
            writer.write_event(e).unwrap();
        }
        writer.flush().unwrap();

        let mut total = 0;
        for i in 0..10 {
            let f = rotating_file(&base, i);
            if std::path::Path::new(&f).exists() {
                total += read_trace_events(&f).len();
            }
        }
        assert_eq!(total, 3);
    }
}
