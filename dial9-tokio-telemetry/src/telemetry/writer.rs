use crate::telemetry::events::{RawEvent, TelemetryEvent};
use crate::telemetry::format::{self, PollStartEvent, PollEndEvent, WorkerParkEvent,
    WorkerUnparkEvent, QueueSampleEvent, TaskSpawnEvent, TaskTerminateEvent, WakeEventEvent};
use dial9_trace_format::encoder::Encoder;
use dial9_trace_format::InternedString;
use std::collections::{HashMap, VecDeque};
use std::fs::{self, File};
use std::io::{self, BufWriter, Write};
use std::panic::Location;
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
    /// Write a raw event from the worker thread buffer. The writer handles
    /// interning spawn locations and encoding to the wire format.
    fn write_raw_event(&mut self, raw: RawEvent) -> io::Result<()>;

    /// Write a pre-constructed TelemetryEvent (used by the CPU profiling path).
    fn write_event(&mut self, event: &TelemetryEvent) -> io::Result<()>;

    /// Write a group of pre-constructed events atomically — all events land in
    /// the same file. Used by the CPU profiling path for def+event batches.
    fn write_atomic(&mut self, events: &[TelemetryEvent]) -> io::Result<WriteAtomicResult>;

    fn flush(&mut self) -> io::Result<()>;

    /// Returns true if the writer rotated to a new file since the last call.
    fn take_rotated(&mut self) -> bool {
        false
    }
}

impl<W: TraceWriter + ?Sized> TraceWriter for Box<W> {
    fn write_raw_event(&mut self, raw: RawEvent) -> io::Result<()> {
        (**self).write_raw_event(raw)
    }
    fn write_event(&mut self, event: &TelemetryEvent) -> io::Result<()> {
        (**self).write_event(event)
    }
    fn write_atomic(&mut self, events: &[TelemetryEvent]) -> io::Result<WriteAtomicResult> {
        (**self).write_atomic(events)
    }
    fn flush(&mut self) -> io::Result<()> {
        (**self).flush()
    }
    fn take_rotated(&mut self) -> bool {
        (**self).take_rotated()
    }
}

/// A writer that discards all events. Useful for benchmarking hook overhead
/// without I/O costs.
pub struct NullWriter;

impl TraceWriter for NullWriter {
    fn write_raw_event(&mut self, _raw: RawEvent) -> io::Result<()> {
        Ok(())
    }
    fn write_event(&mut self, _event: &TelemetryEvent) -> io::Result<()> {
        Ok(())
    }
    fn write_atomic(&mut self, _events: &[TelemetryEvent]) -> io::Result<WriteAtomicResult> {
        Ok(WriteAtomicResult::Written)
    }
    fn flush(&mut self) -> io::Result<()> {
        Ok(())
    }
}

// ── Spawn-location interning ────────────────────────────────────────────────

/// Caches Location pointer → InternedString mapping. The actual string interning
/// is done by the encoder's string pool; this just avoids formatting the location
/// string on every event.
struct SpawnLocationCache {
    intern_map: HashMap<usize, InternedString>,
}

impl SpawnLocationCache {
    fn new() -> Self {
        Self {
            intern_map: HashMap::new(),
        }
    }

    fn intern<W: Write>(
        &mut self,
        location: &'static Location<'static>,
        encoder: &mut Encoder<W>,
    ) -> io::Result<InternedString> {
        let ptr = location as *const Location<'static> as usize;
        if let Some(&id) = self.intern_map.get(&ptr) {
            return Ok(id);
        }
        let loc_str = format!(
            "{}:{}:{}",
            location.file(),
            location.line(),
            location.column()
        );
        let id = encoder.intern_string(&loc_str)?;
        self.intern_map.insert(ptr, id);
        Ok(id)
    }
}

// ── CountingWriter ──────────────────────────────────────────────────────────

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

// ── RotatingWriter ──────────────────────────────────────────────────────────

/// A writer that rotates trace files to bound disk usage.
///
/// Owns the encoder and spawn-location interning state. Handles converting
/// `RawEvent`s to wire format, interning spawn locations via the encoder's
/// string pool.
pub struct RotatingWriter {
    base_path: PathBuf,
    max_file_size: u64,
    max_total_size: u64,
    files: VecDeque<(PathBuf, u64)>,
    total_size: u64,
    encoder: Encoder<CountingWriter<BufWriter<File>>>,
    next_index: u32,
    stopped: bool,
    rotated: bool,
    has_events_in_current_file: bool,
    locations: SpawnLocationCache,
}

impl RotatingWriter {
    fn open_encoder(path: &Path) -> io::Result<(Encoder<CountingWriter<BufWriter<File>>>, u64)> {
        let file = File::create(path)?;
        let counting = CountingWriter::new(BufWriter::new(file));
        let mut encoder = Encoder::new_to(counting)?;
        format::register_schemas(&mut encoder)?;
        encoder.flush()?;
        let preamble_size = encoder.as_inner().count;
        Ok((encoder, preamble_size))
    }

    pub fn new(
        base_path: impl Into<PathBuf>,
        max_file_size: u64,
        max_total_size: u64,
    ) -> io::Result<Self> {
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
            locations: SpawnLocationCache::new(),
        })
    }

    /// Create a writer that writes to a single file with no rotation or eviction.
    pub fn single_file(path: impl Into<PathBuf>) -> io::Result<Self> {
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
            locations: SpawnLocationCache::new(),
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

    fn rotate(&mut self) -> io::Result<()> {
        self.encoder.flush()?;
        let closing_size = self.bytes_written();
        if let Some(last) = self.files.back_mut() {
            last.1 = closing_size;
        }
        self.total_size = self.files.iter().map(|(_, s)| s).sum();

        let new_path = Self::file_path(&self.base_path, self.next_index);
        self.next_index += 1;
        let (encoder, preamble_size) = Self::open_encoder(&new_path)?;
        self.encoder = encoder;
        self.total_size += preamble_size;
        self.files.push_back((new_path, preamble_size));
        self.rotated = true;
        self.has_events_in_current_file = false;
        self.locations.intern_map.clear();

        self.evict_oldest()?;
        Ok(())
    }

    fn evict_oldest(&mut self) -> io::Result<()> {
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

    fn maybe_rotate(&mut self) -> io::Result<()> {
        if self.stopped {
            return Ok(());
        }
        if self.bytes_written() >= self.max_file_size && self.has_events_in_current_file {
            self.rotate()?;
        }
        Ok(())
    }

    fn check_budget(&mut self) -> io::Result<()> {
        let current = self.bytes_written();
        let prev_total: u64 = self.files.iter().rev().skip(1).map(|(_, s)| s).sum();
        if prev_total + current > self.max_total_size {
            self.encoder.flush()?;
            self.stopped = true;
        }
        Ok(())
    }

    fn write_event_inner(&mut self, event: &TelemetryEvent) -> io::Result<()> {
        if self.stopped {
            return Ok(());
        }
        self.maybe_rotate()?;
        if self.stopped {
            return Ok(());
        }
        format::write_event(&mut self.encoder, event)?;
        self.has_events_in_current_file = true;
        self.check_budget()
    }

    /// Resolve a RawEvent: intern locations, emit defs, encode the event.
    /// Handles rotation internally — defs and event always land in the same file.
    fn write_raw_event_inner(&mut self, raw: RawEvent) -> io::Result<()> {
        if self.stopped {
            return Ok(());
        }
        self.maybe_rotate()?;
        if self.stopped {
            return Ok(());
        }

        // Encode directly to derive structs — no TelemetryEvent intermediate.
        match raw {
            RawEvent::PollStart {
                timestamp_nanos,
                worker_id,
                worker_local_queue_depth,
                task_id,
                location,
            } => {
                let spawn_loc_id = self.locations.intern(location, &mut self.encoder)?;
                self.encoder.write(&PollStartEvent {
                    timestamp_ns: timestamp_nanos,
                    worker_id: worker_id as u8,
                    local_queue: worker_local_queue_depth as u8,
                    task_id,
                    spawn_loc_id,
                })?;
            }
            RawEvent::TaskSpawn {
                timestamp_nanos,
                task_id,
                location,
            } => {
                let spawn_loc_id = self.locations.intern(location, &mut self.encoder)?;
                self.encoder.write(&TaskSpawnEvent {
                    timestamp_ns: timestamp_nanos,
                    task_id,
                    spawn_loc_id,
                })?;
            }
            RawEvent::PollEnd {
                timestamp_nanos,
                worker_id,
            } => {
                self.encoder.write(&PollEndEvent {
                    timestamp_ns: timestamp_nanos,
                    worker_id: worker_id as u8,
                })?;
            }
            RawEvent::WorkerPark {
                timestamp_nanos,
                worker_id,
                worker_local_queue_depth,
                cpu_time_nanos,
            } => {
                self.encoder.write(&WorkerParkEvent {
                    timestamp_ns: timestamp_nanos,
                    worker_id: worker_id as u8,
                    local_queue: worker_local_queue_depth as u8,
                    cpu_time_ns: cpu_time_nanos,
                })?;
            }
            RawEvent::WorkerUnpark {
                timestamp_nanos,
                worker_id,
                worker_local_queue_depth,
                cpu_time_nanos,
                sched_wait_delta_nanos,
            } => {
                self.encoder.write(&WorkerUnparkEvent {
                    timestamp_ns: timestamp_nanos,
                    worker_id: worker_id as u8,
                    local_queue: worker_local_queue_depth as u8,
                    cpu_time_ns: cpu_time_nanos,
                    sched_wait_ns: sched_wait_delta_nanos,
                })?;
            }
            RawEvent::QueueSample {
                timestamp_nanos,
                global_queue_depth,
            } => {
                self.encoder.write(&QueueSampleEvent {
                    timestamp_ns: timestamp_nanos,
                    global_queue: global_queue_depth as u8,
                })?;
            }
            RawEvent::TaskTerminate {
                timestamp_nanos,
                task_id,
            } => {
                self.encoder.write(&TaskTerminateEvent {
                    timestamp_ns: timestamp_nanos,
                    task_id,
                })?;
            }
            RawEvent::WakeEvent {
                timestamp_nanos,
                waker_task_id,
                woken_task_id,
                target_worker,
            } => {
                self.encoder.write(&WakeEventEvent {
                    timestamp_ns: timestamp_nanos,
                    waker_task_id,
                    woken_task_id,
                    target_worker,
                })?;
            }
        }
        self.has_events_in_current_file = true;
        self.check_budget()
    }
}

impl TraceWriter for RotatingWriter {
    fn write_raw_event(&mut self, raw: RawEvent) -> io::Result<()> {
        self.write_raw_event_inner(raw)
    }

    fn write_event(&mut self, event: &TelemetryEvent) -> io::Result<()> {
        self.write_event_inner(event)
    }

    fn write_atomic(&mut self, events: &[TelemetryEvent]) -> io::Result<WriteAtomicResult> {
        if self.stopped {
            return Ok(WriteAtomicResult::Written);
        }
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

    fn flush(&mut self) -> io::Result<()> {
        if !self.stopped {
            self.encoder.flush()?;
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
    use crate::telemetry::task_metadata::UNKNOWN_TASK_ID;
    use dial9_trace_format::InternedString;
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

        writer.write_event(&park_event()).unwrap();
        writer.flush().unwrap();

        let events = read_trace_events(path.to_str().unwrap());
        assert_eq!(events.len(), 1);
    }

    #[test]
    fn test_write_raw_event_with_location() {
        let dir = TempDir::new().unwrap();
        let path = dir.path().join("test_raw.bin");
        let mut writer = RotatingWriter::single_file(&path).unwrap();

        let loc = std::panic::Location::caller();
        writer
            .write_raw_event(RawEvent::PollStart {
                timestamp_nanos: 1000,
                worker_id: 0,
                worker_local_queue_depth: 2,
                task_id: crate::telemetry::task_metadata::TaskId::from_u32(1),
                location: loc,
            })
            .unwrap();
        writer.flush().unwrap();

        let mut reader = TraceReader::new(path.to_str().unwrap()).unwrap();
        let mut all = Vec::new();
        while let Some(ev) = reader.read_raw_event().unwrap() {
            all.push(ev);
        }
        // Should have just PollStart (no SpawnLocationDef — interning is in the string pool)
        assert_eq!(all.len(), 1);
        assert!(matches!(all[0], TelemetryEvent::PollStart { .. }));
    }

    #[test]
    fn test_write_raw_event_deduplicates_interning() {
        let dir = TempDir::new().unwrap();
        let path = dir.path().join("test_dedup.bin");
        let mut writer = RotatingWriter::single_file(&path).unwrap();

        let loc = std::panic::Location::caller();
        for _ in 0..3 {
            writer
                .write_raw_event(RawEvent::PollStart {
                    timestamp_nanos: 1000,
                    worker_id: 0,
                    worker_local_queue_depth: 0,
                    task_id: crate::telemetry::task_metadata::TaskId::from_u32(1),
                    location: loc,
                })
                .unwrap();
        }
        writer.flush().unwrap();

        let mut reader = TraceReader::new(path.to_str().unwrap()).unwrap();
        let mut polls = 0;
        while let Some(ev) = reader.read_raw_event().unwrap() {
            if matches!(ev, TelemetryEvent::PollStart { .. }) {
                polls += 1;
            }
        }
        assert_eq!(polls, 3);
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

        assert!(!std::path::Path::new(&rotating_file(&base, 0)).exists());
    }

    #[test]
    fn test_rotating_writer_stops_when_over_budget() {
        let dir = TempDir::new().unwrap();
        let base = dir.path().join("trace");
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
                spawn_loc_id: InternedString::default(),
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
