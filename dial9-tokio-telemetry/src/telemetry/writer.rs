use dial9_trace_format::encoder::Encoder;
use dial9_trace_format::{InternedString, StackFrames};

use crate::telemetry::events::RawEvent;
use crate::telemetry::format::{
    CpuSampleEvent, PollEndEvent, PollStartEvent, QueueSampleEvent, SegmentMetadataEvent,
    TaskSpawnEvent, TaskTerminateEvent, WakeEventEvent, WorkerParkEvent, WorkerUnparkEvent,
};
use std::collections::{HashMap, VecDeque};
use std::fs::{self, File};
use std::io::BufWriter;
use std::path::{Path, PathBuf};

pub trait TraceWriter: Send {
    fn write_event(&mut self, event: &RawEvent) -> std::io::Result<()>;
    fn flush(&mut self) -> std::io::Result<()>;
    /// Returns true if the writer rotated to a new file since the last call to this method.
    fn take_rotated(&mut self) -> bool {
        false
    }
    /// Finalize the writer: flush, rename `.active` → `.bin`, and prevent
    /// further writes. This is a terminal operation — the writer becomes
    /// inert afterward.
    fn finalize(&mut self) -> std::io::Result<()> {
        self.flush()
    }
    /// Write a batch of events atomically — rotation is deferred until after
    /// the entire batch, so all events land in the same file.
    fn write_event_batch(&mut self, events: &[RawEvent]) -> std::io::Result<()> {
        for event in events {
            self.write_event(event)?;
        }
        Ok(())
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
    fn finalize(&mut self) -> std::io::Result<()> {
        (**self).finalize()
    }
    fn write_event_batch(&mut self, events: &[RawEvent]) -> std::io::Result<()> {
        (**self).write_event_batch(events)
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
    /// Tracks (path, size) of closed files oldest-first. The active file is
    /// not in this list — its size comes from `encoder.bytes_written()`.
    closed_files: VecDeque<(PathBuf, u64)>,
    /// Path of the currently active (being-written) file.
    active_path: PathBuf,
    state: WriterState,
    next_index: u32,
    /// Set after rotation; cleared by `take_rotated()`.
    did_rotate: bool,
    /// Optional metadata written at the start of each segment.
    segment_metadata: Option<Vec<(String, String)>>,
    /// Cache from Location → formatted string, to avoid
    /// reformatting on every event.
    formatted_locations: HashMap<std::panic::Location<'static>, String>,
    /// Events silently dropped because the writer was finished/stopped.
    dropped_events: usize,
}

// the write side is obviously marge larger than the `Finished` size so clippy warns on this
// but we don't want to force going through a pointer every time we want to write.
#[allow(clippy::large_enum_variant)]
enum WriterState {
    Active(Encoder<BufWriter<File>>),
    /// Writer has been finalized or stopped — no encoder, no fd, no writes.
    Finished,
}

#[bon::bon]
impl RotatingWriter {
    /// Create a new rotating writer. For additional options like `segment_metadata`,
    /// use [`RotatingWriter::builder()`].
    pub fn new(
        base_path: impl Into<PathBuf>,
        max_file_size: u64,
        max_total_size: u64,
    ) -> std::io::Result<Self> {
        Self::create(base_path, max_file_size, max_total_size, None)
    }

    #[builder(builder_type = RotatingWriterBuilder, finish_fn = build)]
    pub fn builder(
        base_path: impl Into<PathBuf>,
        max_file_size: u64,
        max_total_size: u64,
        segment_metadata: Option<Vec<(String, String)>>,
    ) -> std::io::Result<Self> {
        Self::create(base_path, max_file_size, max_total_size, segment_metadata)
    }

    fn create(
        base_path: impl Into<PathBuf>,
        max_file_size: u64,
        max_total_size: u64,
        segment_metadata: Option<Vec<(String, String)>>,
    ) -> std::io::Result<Self> {
        let base_path = base_path.into();
        if let Some(parent) = base_path.parent() {
            fs::create_dir_all(parent)?;
        }
        let first_path = Self::active_path(&base_path, 0);
        let file = File::create(&first_path)?;
        let writer = BufWriter::new(file);
        let encoder = Encoder::new_to(writer)?;

        let mut w = Self {
            base_path,
            max_file_size,
            max_total_size,
            closed_files: VecDeque::new(),
            active_path: first_path,
            state: WriterState::Active(encoder),
            next_index: 1,
            did_rotate: false,
            segment_metadata,
            formatted_locations: HashMap::new(),
            dropped_events: 0,
        };
        w.write_segment_metadata()?;
        Ok(w)
    }

    /// Create a writer that writes to a single file with no rotation or eviction.
    /// The file is created at exactly the given path.
    ///
    /// Note: This API does not allow the ability to provide custom segment metadata.
    pub fn single_file(path: impl Into<PathBuf>) -> std::io::Result<Self> {
        let path = path.into();
        if let Some(parent) = path.parent() {
            fs::create_dir_all(parent)?;
        }
        let file = File::create(&path)?;
        let writer = BufWriter::new(file);

        let mut w = Self {
            base_path: path.clone(),
            max_file_size: u64::MAX,
            max_total_size: u64::MAX,
            closed_files: VecDeque::new(),
            active_path: path,
            state: WriterState::Active(Encoder::new_to(writer)?),
            next_index: 1,
            did_rotate: false,
            segment_metadata: None,
            formatted_locations: HashMap::new(),
            dropped_events: 0,
        };
        w.write_segment_metadata()?;
        Ok(w)
    }

    /// The base path used for trace segment files.
    pub fn base_path(&self) -> &Path {
        &self.base_path
    }

    /// Write the segment metadata event. Always writes — uses configured
    /// entries or empty vec if none set. Called on construction and rotation.
    fn write_segment_metadata(&mut self) -> std::io::Result<()> {
        let WriterState::Active(encoder) = &mut self.state else {
            return Ok(());
        };
        let entries = self.segment_metadata.clone().unwrap_or_default();
        let timestamp_ns = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap_or_default()
            .as_nanos() as u64;
        encoder.write(&SegmentMetadataEvent {
            timestamp_ns,
            entries,
        })?;
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
        let WriterState::Active(encoder) = &mut self.state else {
            return Ok(());
        };
        encoder.flush()?;
        // Seal the current segment: snapshot size and rename .active → .bin
        let closed_size = encoder.bytes_written();
        let sealed = Self::file_path(&self.base_path, self.next_index - 1);
        fs::rename(&self.active_path, &sealed)?;
        self.closed_files.push_back((sealed, closed_size));

        let new_path = Self::active_path(&self.base_path, self.next_index);
        self.next_index += 1;
        let file = File::create(&new_path)?;
        let writer = BufWriter::new(file);
        self.state = WriterState::Active(Encoder::new_to(writer)?);
        self.active_path = new_path;
        self.did_rotate = true;
        self.write_segment_metadata()?;

        tracing::info!(
            segment_index = self.next_index - 1,
            "rotated to new trace segment"
        );
        self.evict_oldest()?;
        Ok(())
    }

    /// Total size across all files (closed + active).
    fn total_size(&self) -> u64 {
        let closed: u64 = self.closed_files.iter().map(|(_, s)| s).sum();
        let active = match &self.state {
            WriterState::Active(encoder) => encoder.bytes_written(),
            WriterState::Finished => 0,
        };
        closed + active
    }

    fn evict_oldest(&mut self) -> std::io::Result<()> {
        // Always keep at least the current file.
        while self.total_size() > self.max_total_size && !self.closed_files.is_empty() {
            if let Some((path, _size)) = self.closed_files.pop_front()
                && let Err(e) = fs::remove_file(&path)
                && e.kind() != std::io::ErrorKind::NotFound
            {
                tracing::warn!("failed to evict old trace segment {}: {e}", path.display());
            }
        }
        // If even the current file alone exceeds total budget, stop writing.
        if self.total_size() > self.max_total_size {
            self.state = WriterState::Finished;
        }
        Ok(())
    }

    /// Intern a `&'static Location` via the encoder's string pool, using
    /// the location_cache to avoid reformatting the same pointer repeatedly.
    fn intern_location(
        encoder: &mut Encoder<BufWriter<File>>,
        cache: &mut HashMap<std::panic::Location<'static>, String>,
        location: &'static std::panic::Location<'static>,
    ) -> std::io::Result<InternedString> {
        let s = cache
            .entry(*location)
            .or_insert_with(|| location.to_string());
        encoder.intern_string(s)
    }

    /// Resolve a RawEvent and write it. Rotation is deferred until after
    /// the complete logical unit (defs + event) is written, so they always
    /// land in the same file.
    fn write_resolved(&mut self, event: &RawEvent) -> std::io::Result<()> {
        self.write_resolved_no_rotate(event)?;
        self.maybe_rotate()?;
        Ok(())
    }

    /// Write a resolved event without triggering rotation.
    fn write_resolved_no_rotate(&mut self, event: &RawEvent) -> std::io::Result<()> {
        let WriterState::Active(encoder) = &mut self.state else {
            self.dropped_events += 1;
            return Ok(());
        };
        match event {
            RawEvent::PollStart {
                timestamp_nanos,
                worker_id,
                worker_local_queue_depth,
                task_id,
                location,
            } => {
                let spawn_loc =
                    Self::intern_location(encoder, &mut self.formatted_locations, location)?;
                encoder.write(&PollStartEvent {
                    timestamp_ns: *timestamp_nanos,
                    worker_id: *worker_id,
                    local_queue: *worker_local_queue_depth as u8,
                    task_id: *task_id,
                    spawn_loc,
                })
            }
            RawEvent::PollEnd {
                timestamp_nanos,
                worker_id,
            } => encoder.write(&PollEndEvent {
                timestamp_ns: *timestamp_nanos,
                worker_id: *worker_id,
            }),
            RawEvent::WorkerPark {
                timestamp_nanos,
                worker_id,
                worker_local_queue_depth,
                cpu_time_nanos,
            } => encoder.write(&WorkerParkEvent {
                timestamp_ns: *timestamp_nanos,
                worker_id: *worker_id,
                local_queue: *worker_local_queue_depth as u8,
                cpu_time_ns: *cpu_time_nanos,
            }),
            RawEvent::WorkerUnpark {
                timestamp_nanos,
                worker_id,
                worker_local_queue_depth,
                cpu_time_nanos,
                sched_wait_delta_nanos,
            } => encoder.write(&WorkerUnparkEvent {
                timestamp_ns: *timestamp_nanos,
                worker_id: *worker_id,
                local_queue: *worker_local_queue_depth as u8,
                cpu_time_ns: *cpu_time_nanos,
                sched_wait_ns: *sched_wait_delta_nanos,
            }),
            RawEvent::QueueSample {
                timestamp_nanos,
                global_queue_depth,
            } => encoder.write(&QueueSampleEvent {
                timestamp_ns: *timestamp_nanos,
                global_queue: *global_queue_depth as u8,
            }),
            RawEvent::TaskSpawn {
                timestamp_nanos,
                task_id,
                location,
            } => {
                let spawn_loc =
                    Self::intern_location(encoder, &mut self.formatted_locations, location)?;
                encoder.write(&TaskSpawnEvent {
                    timestamp_ns: *timestamp_nanos,
                    task_id: *task_id,
                    spawn_loc,
                })
            }
            RawEvent::TaskTerminate {
                timestamp_nanos,
                task_id,
            } => encoder.write(&TaskTerminateEvent {
                timestamp_ns: *timestamp_nanos,
                task_id: *task_id,
            }),
            RawEvent::WakeEvent {
                timestamp_nanos,
                waker_task_id,
                woken_task_id,
                target_worker,
            } => encoder.write(&WakeEventEvent {
                timestamp_ns: *timestamp_nanos,
                waker_task_id: *waker_task_id,
                woken_task_id: *woken_task_id,
                target_worker: *target_worker,
            }),
            RawEvent::CpuSample(data) => {
                let thread_name = match &data.thread_name {
                    Some(name) => encoder.intern_string(name.as_str()),
                    None => encoder.intern_string("<no thread name>"),
                }?;

                encoder.write(&CpuSampleEvent {
                    timestamp_ns: data.timestamp_nanos,
                    worker_id: data.worker_id,
                    tid: data.tid,
                    source: data.source,
                    thread_name,
                    callchain: StackFrames(data.callchain.clone()),
                })
            }
        }?;
        Ok(())
    }

    /// Rotate if the current file exceeds max_file_size.
    /// Called after writing a complete logical unit (def + event).
    fn maybe_rotate(&mut self) -> std::io::Result<()> {
        if let WriterState::Active(encoder) = &self.state
            && encoder.bytes_written() > self.max_file_size
        {
            self.rotate()?;
        }
        Ok(())
    }
}

impl TraceWriter for RotatingWriter {
    fn write_event(&mut self, event: &RawEvent) -> std::io::Result<()> {
        self.write_resolved(event)
    }

    fn write_event_batch(&mut self, events: &[RawEvent]) -> std::io::Result<()> {
        for event in events {
            self.write_resolved_no_rotate(event)?;
        }
        self.maybe_rotate()?;
        Ok(())
    }

    fn flush(&mut self) -> std::io::Result<()> {
        if let WriterState::Active(encoder) = &mut self.state {
            encoder.flush()?;
        }
        Ok(())
    }

    fn take_rotated(&mut self) -> bool {
        std::mem::take(&mut self.did_rotate)
    }

    fn finalize(&mut self) -> std::io::Result<()> {
        if matches!(self.state, WriterState::Finished) {
            tracing::warn!("writer is already closed.")
        }
        self.flush()?;
        // Rename .active → .bin for the current segment (if it has .active suffix)
        if self
            .active_path
            .extension()
            .is_some_and(|ext| ext == "active")
        {
            let sealed = Self::file_path(&self.base_path, self.next_index - 1);
            fs::rename(&self.active_path, &sealed)?;
            self.active_path = sealed;
        }
        self.state = WriterState::Finished;
        Ok(())
    }
}

impl Drop for RotatingWriter {
    fn drop(&mut self) {
        if self.dropped_events > 0 {
            tracing::info!(
                target: "dial9_telemetry",
                dropped_events = self.dropped_events,
                "RotatingWriter dropped events after finalization"
            );
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::telemetry::{TelemetryEvent, format};
    use std::io::Read;
    use tempfile::TempDir;

    fn park_event() -> RawEvent {
        RawEvent::WorkerPark {
            timestamp_nanos: 1000,
            worker_id: crate::telemetry::format::WorkerId::from(0usize),
            worker_local_queue_depth: 2,
            cpu_time_nanos: 0,
        }
    }

    fn rotating_file(base: &std::path::Path, i: u32) -> String {
        format!("{}.{}.bin", base.display(), i)
    }

    /// Read all non-metadata events from a trace file.
    fn read_trace_events(path: &str) -> Vec<TelemetryEvent> {
        let data = std::fs::read(path).unwrap();
        format::decode_events_v2(&data)
            .unwrap()
            .into_iter()
            .filter(|e| !matches!(e, TelemetryEvent::SegmentMetadata { .. }))
            .collect()
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

    /// Write one park event to a temp file and return the file size.
    /// This captures the actual overhead (header + schema + event) so tests
    /// don't depend on hardcoded format sizes.
    fn single_event_file_size() -> u64 {
        let dir = TempDir::new().unwrap();
        let path = dir.path().join("probe.bin");
        let mut w = RotatingWriter::single_file(&path).unwrap();
        w.write_event(&park_event()).unwrap();
        w.flush().unwrap();
        std::fs::metadata(&path).unwrap().len()
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
        assert!(
            metadata.len() > 0,
            "file should not be empty after writing an event"
        );
    }

    #[test]
    fn test_write_batch_sizes() {
        let dir = TempDir::new().unwrap();
        let path = dir.path().join("test_batch_v2.bin");
        let mut writer = RotatingWriter::single_file(&path).unwrap();

        let one_event_size = single_event_file_size();

        for _ in 0..2 {
            writer.write_event(&park_event()).unwrap();
        }
        writer.flush().unwrap();

        let metadata = std::fs::metadata(&path).unwrap();
        // Two events should be larger than one event
        assert!(metadata.len() > one_event_size);
    }

    #[test]
    fn test_binary_format_header() {
        let dir = TempDir::new().unwrap();
        let path = dir.path().join("test_format_v2.bin");
        let writer = RotatingWriter::single_file(&path).unwrap();
        drop(writer);

        let mut file = std::fs::File::open(&path).unwrap();
        let mut magic = [0u8; 4];
        file.read_exact(&mut magic).unwrap();
        assert_eq!(&magic, b"TRC\0");
    }

    #[test]
    fn test_rotating_writer_creation() {
        let dir = TempDir::new().unwrap();
        let base = dir.path().join("trace");
        let mut writer = RotatingWriter::new(&base, 1024, 4096).unwrap();
        writer.finalize().unwrap();

        let events = read_trace_events(&rotating_file(&base, 0));
        assert_eq!(events.len(), 0);
    }

    #[test]
    fn test_rotating_writer_rotation() {
        let dir = TempDir::new().unwrap();
        let base = dir.path().join("trace");
        // Set max_file_size to fit ~1 event so rotation triggers quickly
        let one_event = single_event_file_size();
        let mut writer = RotatingWriter::new(&base, one_event, 100_000).unwrap();

        for _ in 0..3 {
            writer.write_event(&park_event()).unwrap();
        }
        writer.finalize().unwrap();

        // All 3 events should be readable across rotated files
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
    fn test_rotating_writer_eviction() {
        let dir = TempDir::new().unwrap();
        let base = dir.path().join("trace");
        let one_event = single_event_file_size();
        let max_file_size = one_event;
        let max_total_size = max_file_size * 3;
        let mut writer = RotatingWriter::new(&base, max_file_size, max_total_size).unwrap();

        for _ in 0..10 {
            writer.write_event(&park_event()).unwrap();
        }
        writer.finalize().unwrap();

        // Key invariant: total disk usage stays within budget
        assert!(total_disk_usage(dir.path()) <= max_total_size);

        // Oldest files should be evicted
        assert!(!std::path::Path::new(&rotating_file(&base, 0)).exists());
    }

    #[test]
    fn test_rotating_writer_stops_when_over_budget() {
        let dir = TempDir::new().unwrap();
        let base = dir.path().join("trace");
        let one_event = single_event_file_size();
        // Small file size to force rotation, total budget fits ~1 file
        let max_file_size = one_event;
        let max_total_size = one_event + 5;
        let mut writer = RotatingWriter::new(&base, max_file_size, max_total_size).unwrap();

        for _ in 0..100 {
            writer.write_event(&park_event()).unwrap();
        }
        writer.finalize().unwrap();

        // Should have stopped writing — total events across all files < 100
        let total: usize = (0..100)
            .map(|i| {
                let f = rotating_file(&base, i);
                if std::path::Path::new(&f).exists() {
                    read_trace_events(&f).len()
                } else {
                    0
                }
            })
            .sum();
        assert!(
            total < 100,
            "should have stopped writing, got {total} events"
        );
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
            if matches!(writer.state, WriterState::Finished) {
                panic!(
                    "Writer stopped at event {i}! total_size={}, max_total_size={}, \
                     closed_files={}. \
                     write_event_inner should try eviction before stopping.",
                    writer.total_size(),
                    max_total_size,
                    writer.closed_files.len()
                );
            }
        }
    }

    #[test]
    fn test_rotating_writer_file_naming() {
        let dir = TempDir::new().unwrap();
        let base = dir.path().join("trace");
        let one_event = single_event_file_size();
        let mut writer = RotatingWriter::new(&base, one_event, 100_000).unwrap();

        for _ in 0..5 {
            writer.write_event(&park_event()).unwrap();
        }
        writer.finalize().unwrap();

        // Should have created multiple files with sequential naming
        assert!(
            std::path::Path::new(&rotating_file(&base, 0)).exists(),
            "File 0 should exist"
        );
        // All events should be readable
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
        assert_eq!(total, 5);
    }

    #[test]
    fn test_write_batch_across_rotation_boundary() {
        let dir = TempDir::new().unwrap();
        let base = dir.path().join("trace");
        let one_event = single_event_file_size();
        let mut writer = RotatingWriter::new(&base, one_event, 100_000).unwrap();

        for _ in 0..3 {
            writer.write_event(&park_event()).unwrap();
        }
        writer.finalize().unwrap();

        // All 3 events should be readable across the rotated files.
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
    fn test_rotated_files_have_valid_headers() {
        let dir = TempDir::new().unwrap();
        let base = dir.path().join("trace");
        let one_event = single_event_file_size();
        let mut writer = RotatingWriter::new(&base, one_event, 100_000).unwrap();

        for _ in 0..3 {
            writer.write_event(&park_event()).unwrap();
        }
        writer.finalize().unwrap();

        // Each rotated file must be a self-contained, readable trace.
        let total: usize = (0..10)
            .map(|i| {
                let f = rotating_file(&base, i);
                if std::path::Path::new(&f).exists() {
                    read_trace_events(&f).len() // panics if corrupt
                } else {
                    0
                }
            })
            .sum();
        assert_eq!(total, 3);
    }

    #[test]
    fn test_flush_after_stop() {
        let dir = TempDir::new().unwrap();
        let base = dir.path().join("trace");
        // Total budget smaller than one file — stops immediately
        let mut writer = RotatingWriter::new(&base, 10_000, 50).unwrap();

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
        let one_event = single_event_file_size();
        let mut writer = RotatingWriter::new(&base, one_event, 100_000).unwrap();

        let events = [
            RawEvent::WorkerPark {
                timestamp_nanos: 1000,
                worker_id: crate::telemetry::format::WorkerId::from(0usize),
                worker_local_queue_depth: 2,
                cpu_time_nanos: 0,
            },
            RawEvent::WorkerPark {
                timestamp_nanos: 2000,
                worker_id: crate::telemetry::format::WorkerId::from(1usize),
                worker_local_queue_depth: 0,
                cpu_time_nanos: 0,
            },
            RawEvent::WorkerUnpark {
                timestamp_nanos: 3000,
                worker_id: crate::telemetry::format::WorkerId::from(0usize),
                worker_local_queue_depth: 2,
                cpu_time_nanos: 0,
                sched_wait_delta_nanos: 0,
            },
        ];
        for e in &events {
            writer.write_event(e).unwrap();
        }
        writer.finalize().unwrap();

        // All events should be readable across files.
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
    fn test_event_exactly_on_max_file_size_boundary() {
        let dir = TempDir::new().unwrap();
        let base = dir.path().join("trace");
        let one_event = single_event_file_size();
        // Exactly fits one event file — second event triggers rotation
        let mut writer = RotatingWriter::new(&base, one_event, 100_000).unwrap();

        for _ in 0..2 {
            writer.write_event(&park_event()).unwrap();
        }
        writer.finalize().unwrap();

        // Both events readable across files
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
        assert_eq!(total, 2);
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
        let one_event = single_event_file_size();
        let mut writer = RotatingWriter::new(&base, one_event, 100_000).unwrap();

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
    fn test_finalize_renames_current_file() {
        let dir = TempDir::new().unwrap();
        let base = dir.path().join("trace");
        let mut writer = RotatingWriter::new(&base, 1024, 100000).unwrap();
        writer.write_event(&park_event()).unwrap();
        writer.finalize().unwrap();

        assert!(
            dir.path().join("trace.0.bin").exists(),
            "file should be sealed after finalize()"
        );
        assert!(
            !dir.path().join("trace.0.bin.active").exists(),
            "active file should be gone after finalize()"
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
        let base = dir.path().join("trace");
        let mut writer = RotatingWriter::builder()
            .base_path(&base)
            .max_file_size(100_000)
            .max_total_size(100_000)
            .segment_metadata(vec![
                ("service".into(), "checkout-api".into()),
                ("host".into(), "i-0abc123".into()),
            ])
            .build()
            .unwrap();
        writer.write_event(&park_event()).unwrap();
        writer.flush().unwrap();
        writer.finalize().unwrap();

        let all_events =
            format::decode_events_v2(&std::fs::read(format!("{}.0.bin", base.display())).unwrap())
                .unwrap();
        let metadata: Vec<_> = all_events
            .iter()
            .filter_map(|e| match e {
                TelemetryEvent::SegmentMetadata { entries, .. } => Some(entries.clone()),
                _ => None,
            })
            .collect();
        assert_eq!(metadata.len(), 1);
        assert_eq!(
            metadata[0],
            vec![
                ("service".to_string(), "checkout-api".to_string()),
                ("host".to_string(), "i-0abc123".to_string()),
            ]
        );
    }

    #[test]
    fn test_segment_metadata_written_in_every_rotated_file() {
        let dir = TempDir::new().unwrap();
        let base = dir.path().join("trace");
        let one_event = single_event_file_size();
        let mut writer = RotatingWriter::builder()
            .base_path(&base)
            .max_file_size(one_event)
            .max_total_size(100_000)
            .segment_metadata(vec![("k".into(), "v".into())])
            .build()
            .unwrap();

        for _ in 0..5 {
            writer.write_event(&park_event()).unwrap();
        }
        writer.flush().unwrap();
        writer.finalize().unwrap();

        let mut files: Vec<_> = std::fs::read_dir(dir.path())
            .unwrap()
            .filter_map(|e| e.ok())
            .map(|e| e.path())
            .filter(|p| p.extension().is_some_and(|ext| ext == "bin"))
            .collect();
        files.sort();
        assert!(files.len() >= 2, "expected at least 2 files from rotation");

        for file in &files {
            let all_events = format::decode_events_v2(&std::fs::read(file).unwrap()).unwrap();
            let has_metadata = all_events.iter().any(|e| {
                matches!(e, TelemetryEvent::SegmentMetadata { entries, .. }
                    if *entries == vec![("k".to_string(), "v".to_string())])
            });
            assert!(has_metadata, "{}: expected SegmentMetadata", file.display());
        }
    }

    #[test]
    fn test_segment_metadata_empty_entries() {
        let dir = TempDir::new().unwrap();
        let path = dir.path().join("trace.bin");
        let mut writer = RotatingWriter::single_file(&path).unwrap();
        writer.write_event(&park_event()).unwrap();
        writer.flush().unwrap();

        let all_events = format::decode_events_v2(&std::fs::read(&path).unwrap()).unwrap();
        let park_count = all_events
            .iter()
            .filter(|e| matches!(e, TelemetryEvent::WorkerPark { .. }))
            .count();
        assert_eq!(park_count, 1);
        // Metadata should be present with empty entries
        let metadata: Vec<_> = all_events
            .iter()
            .filter_map(|e| match e {
                TelemetryEvent::SegmentMetadata { entries, .. } => Some(entries),
                _ => None,
            })
            .collect();
        assert_eq!(metadata.len(), 1);
        assert!(metadata[0].is_empty());
    }
}
