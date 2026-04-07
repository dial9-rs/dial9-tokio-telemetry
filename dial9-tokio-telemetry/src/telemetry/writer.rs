use dial9_trace_format::encoder::{Encoder, RawEncoder};

use crate::telemetry::collector::Batch;
use crate::telemetry::format::SegmentMetadataEvent;
use std::collections::VecDeque;
use std::fs::{self, File};
use std::io::BufWriter;
use std::path::{Path, PathBuf};

pub trait TraceWriter: Send {
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
    /// Transcode encoded bytes into this writer.
    fn write_encoded_batch(&mut self, batch: &Batch) -> std::io::Result<()>;
    /// Return the current segment metadata entries. Default returns empty.
    fn segment_metadata(&self) -> &[(String, String)] {
        &[]
    }
    /// Replace the segment metadata entries that will be written into the next
    /// rotated segment (e.g. merged static + runtime names). Default is a no-op.
    fn update_segment_metadata(&mut self, _entries: Vec<(String, String)>) {}
    /// Write a `SegmentMetadataEvent` into the current segment. Called before
    /// finalize so that single-segment traces contain runtime→worker mappings.
    /// Default is a no-op.
    fn write_current_segment_metadata(&mut self) -> std::io::Result<()> {
        Ok(())
    }
}

impl<W: TraceWriter + ?Sized> TraceWriter for Box<W> {
    fn flush(&mut self) -> std::io::Result<()> {
        (**self).flush()
    }
    fn take_rotated(&mut self) -> bool {
        (**self).take_rotated()
    }
    fn finalize(&mut self) -> std::io::Result<()> {
        (**self).finalize()
    }
    fn write_encoded_batch(&mut self, batch: &Batch) -> std::io::Result<()> {
        (**self).write_encoded_batch(batch)
    }
    fn segment_metadata(&self) -> &[(String, String)] {
        (**self).segment_metadata()
    }
    fn update_segment_metadata(&mut self, entries: Vec<(String, String)>) {
        (**self).update_segment_metadata(entries)
    }
    fn write_current_segment_metadata(&mut self) -> std::io::Result<()> {
        (**self).write_current_segment_metadata()
    }
}

/// A writer that discards all events. Useful for benchmarking hook overhead
/// without I/O costs.
pub struct NullWriter;

impl TraceWriter for NullWriter {
    fn write_encoded_batch(&mut self, _batch: &Batch) -> std::io::Result<()> {
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
    /// Metadata written at the start of each segment. Updated by the flush
    /// thread to include runtime names alongside any user-provided entries.
    segment_metadata: Vec<(String, String)>,
    /// Events silently dropped because the writer was finished/stopped.
    dropped_events: usize,
    /// Whether any real (non-metadata) events have been written to the current segment.
    /// Reset on rotation; used by `finalize()` to avoid sealing empty segments.
    has_real_events: bool,
}

// the write side is obviously marge larger than the `Finished` size so clippy warns on this
// but we don't want to force going through a pointer every time we want to write.
#[allow(clippy::large_enum_variant)]
enum WriterState {
    Active(RawEncoder<BufWriter<File>>),
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
        Self::create(base_path, max_file_size, max_total_size, Vec::new())
    }

    #[builder(builder_type = RotatingWriterBuilder, finish_fn = build)]
    pub fn builder(
        base_path: impl Into<PathBuf>,
        max_file_size: u64,
        max_total_size: u64,
        segment_metadata: Option<Vec<(String, String)>>,
    ) -> std::io::Result<Self> {
        Self::create(
            base_path,
            max_file_size,
            max_total_size,
            segment_metadata.unwrap_or_default(),
        )
    }

    fn create(
        base_path: impl Into<PathBuf>,
        max_file_size: u64,
        max_total_size: u64,
        segment_metadata: Vec<(String, String)>,
    ) -> std::io::Result<Self> {
        let base_path = base_path.into();
        if let Some(parent) = base_path.parent() {
            fs::create_dir_all(parent)?;
        }
        let first_path = Self::active_path(&base_path, 0);
        let file = File::create(&first_path)?;
        let writer = BufWriter::new(file);
        let raw = Self::write_header_and_metadata(writer, &segment_metadata)?;

        Ok(Self {
            base_path,
            max_file_size,
            max_total_size,
            closed_files: VecDeque::new(),
            active_path: first_path,
            state: WriterState::Active(raw),
            next_index: 1,
            did_rotate: false,
            segment_metadata,
            dropped_events: 0,
            has_real_events: false,
        })
    }

    /// Create a writer that writes to a single file with no rotation or eviction.
    /// The segment is written to `{stem}.0.bin.active` while active, then sealed
    /// to `{stem}.0.bin` on [`finalize`]. The background worker will symbolize
    /// and gzip it to `{stem}.0.bin.gz`.
    ///
    /// Note: This API does not allow the ability to provide custom segment metadata.
    pub fn single_file(path: impl Into<PathBuf>) -> std::io::Result<Self> {
        let path = path.into();
        if let Some(parent) = path.parent() {
            fs::create_dir_all(parent)?;
        }
        let active_path = Self::active_path(&path, 0);
        let file = File::create(&active_path)?;
        let writer = BufWriter::new(file);
        let raw = Self::write_header_and_metadata(writer, &Vec::new())?;

        Ok(Self {
            base_path: path,
            max_file_size: u64::MAX,
            max_total_size: u64::MAX,
            closed_files: VecDeque::new(),
            active_path,
            state: WriterState::Active(raw),
            next_index: 1,
            did_rotate: false,
            segment_metadata: Vec::new(),
            dropped_events: 0,
            has_real_events: false,
        })
    }

    /// The base path used for trace segment files.
    pub fn base_path(&self) -> &Path {
        &self.base_path
    }

    /// The path of the currently active (being-written) segment file.
    pub fn current_active_path(&self) -> &Path {
        &self.active_path
    }

    /// Create an encoder, write the file header and segment metadata, then
    /// convert to a [`RawEncoder`] for the remainder of the file's lifetime.
    fn write_header_and_metadata(
        writer: BufWriter<File>,
        segment_metadata: &[(String, String)],
    ) -> std::io::Result<RawEncoder<BufWriter<File>>> {
        let mut encoder = Encoder::new_to(writer)?;
        let entries = segment_metadata.to_vec();
        let timestamp_ns = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap_or_default()
            .as_nanos() as u64;
        encoder.write(&SegmentMetadataEvent {
            timestamp_ns,
            entries,
        })?;
        Ok(encoder.into_raw_encoder())
    }

    /// Write a `SegmentMetadataEvent` into the current active segment.
    /// Used by `write_current_segment_metadata` to flush runtime metadata
    /// before finalize.
    fn write_segment_metadata(&mut self) -> std::io::Result<()> {
        let WriterState::Active(raw) = &mut self.state else {
            return Ok(());
        };
        let entries = self.segment_metadata.clone();
        let timestamp_ns = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap_or_default()
            .as_nanos() as u64;
        let mut enc = Encoder::new();
        enc.write(&SegmentMetadataEvent {
            timestamp_ns,
            entries,
        })?;
        raw.write_raw(&enc.finish())?;
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
        let WriterState::Active(raw) = &mut self.state else {
            return Ok(());
        };
        raw.flush()?;
        // Seal the current segment: snapshot size and rename .active → .bin
        let closed_size = raw.bytes_written();
        let sealed = Self::file_path(&self.base_path, self.next_index - 1);
        fs::rename(&self.active_path, &sealed)?;
        self.closed_files.push_back((sealed, closed_size));

        let new_path = Self::active_path(&self.base_path, self.next_index);
        self.next_index += 1;
        let file = File::create(&new_path)?;
        let writer = BufWriter::new(file);
        self.state = WriterState::Active(Self::write_header_and_metadata(
            writer,
            &self.segment_metadata,
        )?);
        self.active_path = new_path;
        self.did_rotate = true;
        self.has_real_events = false;

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
            WriterState::Active(raw) => raw.bytes_written(),
            WriterState::Finished => 0,
        };
        closed + active
    }

    fn evict_oldest(&mut self) -> std::io::Result<()> {
        // Always keep at least the current file.
        while self.total_size() > self.max_total_size && !self.closed_files.is_empty() {
            if let Some((path, _size)) = self.closed_files.pop_front() {
                // Try to remove the sealed `.bin` file directly.  If a
                // background worker has already renamed it (e.g. appended an
                // extension like `.gz`), scan the parent directory for any
                // file whose name starts with the original filename so we
                // stay agnostic to future write-back extensions.
                match fs::remove_file(&path) {
                    Ok(()) => {}
                    Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
                        // NOTE: This directory scan is more expensive than a
                        // direct remove, but it keeps us agnostic to whatever
                        // extension the background worker appends. In practice
                        // eviction is infrequent and the directory is small, so
                        // the cost is hopefully negligible.
                        if let Some(file_name) = path.file_name().and_then(|n| n.to_str())
                            && let Some(parent) = path.parent()
                            && let Ok(entries) = fs::read_dir(parent)
                        {
                            for entry in entries.flatten() {
                                let name = entry.file_name();
                                if let Some(name_str) = name.to_str()
                                    && name_str.starts_with(file_name)
                                    && name_str != file_name
                                    && let Err(e2) = fs::remove_file(entry.path())
                                {
                                    tracing::warn!(
                                        "failed to evict old trace segment {}: {e2}",
                                        entry.path().display()
                                    );
                                }
                            }
                        }
                    }
                    Err(e) => {
                        tracing::warn!("failed to evict old trace segment {}: {e}", path.display());
                    }
                }
            }
        }
        // If even the current file alone exceeds total budget, stop writing.
        if self.total_size() > self.max_total_size {
            self.state = WriterState::Finished;
        }
        Ok(())
    }

    /// Rotate if the current file exceeds max_file_size.
    /// Called after writing a complete logical unit (def + event).
    fn maybe_rotate(&mut self) -> std::io::Result<()> {
        if let WriterState::Active(raw) = &self.state
            && raw.bytes_written() > self.max_file_size
        {
            self.rotate()?;
        }
        Ok(())
    }
}

impl TraceWriter for RotatingWriter {
    fn flush(&mut self) -> std::io::Result<()> {
        if let WriterState::Active(raw) = &mut self.state {
            raw.flush()?;
        }
        Ok(())
    }

    fn take_rotated(&mut self) -> bool {
        std::mem::take(&mut self.did_rotate)
    }

    fn segment_metadata(&self) -> &[(String, String)] {
        &self.segment_metadata
    }

    fn update_segment_metadata(&mut self, entries: Vec<(String, String)>) {
        self.segment_metadata = entries;
    }

    fn write_current_segment_metadata(&mut self) -> std::io::Result<()> {
        self.write_segment_metadata()
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
            if self.has_real_events {
                let sealed = Self::file_path(&self.base_path, self.next_index - 1);
                fs::rename(&self.active_path, &sealed)?;
                self.active_path = sealed;
            } else {
                // No real events — just header + metadata. Remove instead of
                // sealing so the background worker doesn't upload an empty segment.
                tracing::debug!(
                    "removing empty final segment {}",
                    self.active_path.display()
                );
                if let Err(e) = fs::remove_file(&self.active_path)
                    && e.kind() != std::io::ErrorKind::NotFound
                {
                    return Err(e);
                }
            }
        }
        self.state = WriterState::Finished;
        Ok(())
    }

    fn write_encoded_batch(&mut self, batch: &Batch) -> std::io::Result<()> {
        let WriterState::Active(raw) = &mut self.state else {
            self.dropped_events += batch.event_count as usize;
            return Ok(());
        };
        if batch.event_count > 0 {
            // Raw-copy the thread-local batch. Each batch is self-contained
            // (starts with its own header), so the next batch's header acts as
            // the reset frame for decoders.
            raw.write_raw(&batch.encoded_bytes)?;
            self.has_real_events = true;
            self.maybe_rotate()?;
        }
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
    use crate::telemetry::format::WorkerParkEvent;
    use crate::telemetry::{TelemetryEvent, format};
    use std::io::Read;
    use tempfile::TempDir;

    /// Encode a single park event into a self-contained batch (header + event),
    /// matching the format produced by ThreadLocalBuffer.
    fn test_batch() -> Batch {
        let mut enc = Encoder::new_to(Vec::new()).unwrap();
        enc.write_infallible(&WorkerParkEvent {
            timestamp_ns: 1000,
            worker_id: crate::telemetry::format::WorkerId::from(0usize),
            local_queue: 2,
            cpu_time_ns: 0,
        });
        Batch {
            encoded_bytes: enc.into_inner(),
            event_count: 1,
        }
    }

    fn rotating_file(base: &std::path::Path, i: u32) -> String {
        format!("{}.{}.bin", base.display(), i)
    }

    /// Read all non-metadata events from a trace file.
    fn read_trace_events(path: &str) -> Vec<TelemetryEvent> {
        let data = std::fs::read(path).unwrap();
        format::decode_events(&data)
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

    /// Write one batch to a temp file and return the file size.
    /// This captures the actual overhead (header + schema + event) so tests
    /// don't depend on hardcoded format sizes.
    fn single_event_file_size() -> u64 {
        let dir = TempDir::new().unwrap();
        let path = dir.path().join("probe.bin");
        let mut w = RotatingWriter::single_file(&path).unwrap();
        w.write_encoded_batch(&test_batch()).unwrap();
        w.flush().unwrap();
        std::fs::metadata(w.current_active_path()).unwrap().len()
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

        writer.write_encoded_batch(&test_batch()).unwrap();
        writer.flush().unwrap();

        let metadata = std::fs::metadata(writer.current_active_path()).unwrap();
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
            writer.write_encoded_batch(&test_batch()).unwrap();
        }
        writer.flush().unwrap();

        let metadata = std::fs::metadata(writer.current_active_path()).unwrap();
        // Two events should be larger than one event
        assert!(metadata.len() > one_event_size);
    }

    #[test]
    fn test_binary_format_header() {
        let dir = TempDir::new().unwrap();
        let path = dir.path().join("test_format_v2.bin");
        let writer = RotatingWriter::single_file(&path).unwrap();
        let active = writer.current_active_path().to_owned();
        drop(writer);

        let mut file = std::fs::File::open(&active).unwrap();
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

        // No real events were written, so finalize removes the empty segment.
        assert!(
            !dir.path().join("trace.0.bin").exists(),
            "empty segment should not be sealed"
        );
        assert!(
            !dir.path().join("trace.0.bin.active").exists(),
            "active file should be removed"
        );
    }

    #[test]
    fn test_rotating_writer_rotation() {
        let dir = TempDir::new().unwrap();
        let base = dir.path().join("trace");
        // Set max_file_size to fit ~1 event so rotation triggers quickly
        let one_event = single_event_file_size();
        let mut writer = RotatingWriter::new(&base, one_event, 100_000).unwrap();

        for _ in 0..3 {
            writer.write_encoded_batch(&test_batch()).unwrap();
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
            writer.write_encoded_batch(&test_batch()).unwrap();
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
            writer.write_encoded_batch(&test_batch()).unwrap();
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

    /// Bug: write_encoded_batch sets stopped=true when total_size slightly exceeds
    /// max_total_size, without attempting eviction. This happens right after
    /// rotate() + evict_oldest() brings total_size just under budget, then the
    /// first batch in the new file pushes it a few bytes over. The writer
    /// permanently stops even though eviction could free space.
    ///
    /// Reproduces the stress test failure: 64-worker runtime with 1MB segments
    /// and 100MB budget stops producing segments after ~100 rotations.
    #[test]
    fn test_writer_stops_on_tiny_overshoot_after_eviction() {
        let dir = TempDir::new().unwrap();
        let base = dir.path().join("trace");
        // Use max_file_size that doesn't evenly divide by batch size,
        // so files end up slightly under max_file_size (with leftover bytes).
        // Over 100 files, these leftovers accumulate and push total_size
        // past max_total_size after eviction.
        let max_file_size = 200;
        let num_files = 100u64;
        let max_total_size = max_file_size * num_files;
        let mut writer = RotatingWriter::new(&base, max_file_size, max_total_size).unwrap();

        // Write many batches. The batch size doesn't divide evenly into
        // (max_file_size - header), so each file wastes a few bytes. After
        // 100 rotations, total_size drifts above max_total_size.
        for i in 0..5000 {
            writer.write_encoded_batch(&test_batch()).unwrap();
            if matches!(writer.state, WriterState::Finished) {
                panic!(
                    "Writer stopped at batch {i}! total_size={}, max_total_size={}, \
                     closed_files={}. \
                     write_encoded_batch should try eviction before stopping.",
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
            writer.write_encoded_batch(&test_batch()).unwrap();
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
            writer.write_encoded_batch(&test_batch()).unwrap();
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
            writer.write_encoded_batch(&test_batch()).unwrap();
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
            writer.write_encoded_batch(&test_batch()).unwrap();
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

        for _ in 0..3 {
            writer.write_encoded_batch(&test_batch()).unwrap();
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
            writer.write_encoded_batch(&test_batch()).unwrap();
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
        writer.write_encoded_batch(&test_batch()).unwrap();
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
        writer.write_encoded_batch(&test_batch()).unwrap();
        writer.write_encoded_batch(&test_batch()).unwrap();
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
        writer.write_encoded_batch(&test_batch()).unwrap();
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
    fn test_finalize_removes_empty_segment_after_rotation() {
        let dir = TempDir::new().unwrap();
        let base = dir.path().join("trace");
        // Small max_file_size so one event triggers rotation.
        let mut writer = RotatingWriter::new(&base, 1, 100_000).unwrap();
        // Write an event — this fills segment 0 and triggers rotation to segment 1.
        writer.write_encoded_batch(&test_batch()).unwrap();
        // Segment 0 is sealed, segment 1 is active with only header + metadata.
        assert!(dir.path().join("trace.0.bin").exists());
        assert!(dir.path().join("trace.1.bin.active").exists());

        // Finalize should remove the empty segment 1 instead of sealing it.
        writer.finalize().unwrap();
        assert!(
            !dir.path().join("trace.1.bin").exists(),
            "empty segment should not be sealed"
        );
        assert!(
            !dir.path().join("trace.1.bin.active").exists(),
            "empty active file should be removed"
        );
        // Segment 0 should still exist.
        assert!(dir.path().join("trace.0.bin").exists());
    }

    #[test]
    fn test_single_file_no_active_suffix() {
        let dir = TempDir::new().unwrap();
        let path = dir.path().join("test.bin");
        let mut writer = RotatingWriter::single_file(&path).unwrap();
        writer.write_encoded_batch(&test_batch()).unwrap();
        writer.flush().unwrap();
        writer.finalize().unwrap();

        // single_file seals to test.0.bin after finalize, no leftover .active
        assert!(dir.path().join("test.0.bin").exists());
        assert!(!dir.path().join("test.0.bin.active").exists());
    }

    #[test]
    fn test_single_file_sealed_segment_discoverable_by_worker() {
        use crate::background_task::sealed::find_sealed_segments;

        let dir = TempDir::new().unwrap();
        let path = dir.path().join("trace.bin");
        let mut writer = RotatingWriter::single_file(&path).unwrap();
        writer.write_encoded_batch(&test_batch()).unwrap();
        writer.flush().unwrap();
        writer.finalize().unwrap();

        let segments = find_sealed_segments(dir.path(), "trace").unwrap();
        assert_eq!(
            segments.len(),
            1,
            "worker should find exactly one sealed segment"
        );
        assert_eq!(segments[0].path, dir.path().join("trace.0.bin"));
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
        writer.write_encoded_batch(&test_batch()).unwrap();
        writer.flush().unwrap();
        writer.finalize().unwrap();

        let all_events =
            format::decode_events(&std::fs::read(format!("{}.0.bin", base.display())).unwrap())
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
            writer.write_encoded_batch(&test_batch()).unwrap();
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
            let all_events = format::decode_events(&std::fs::read(file).unwrap()).unwrap();
            let has_metadata = all_events.iter().any(|e| {
                matches!(e, TelemetryEvent::SegmentMetadata { entries, .. }
                    if *entries == vec![("k".to_string(), "v".to_string())])
            });
            assert!(has_metadata, "{}: expected SegmentMetadata", file.display());
        }
    }

    #[test]
    fn test_dynamic_metadata_merged_on_rotation() {
        let dir = TempDir::new().unwrap();
        let base = dir.path().join("trace");
        let one_event = single_event_file_size();
        let mut writer = RotatingWriter::builder()
            .base_path(&base)
            .max_file_size(one_event)
            .max_total_size(100_000)
            .segment_metadata(vec![("service".into(), "myapp".into())])
            .build()
            .unwrap();

        // Simulate the flush thread merging static + runtime→worker entries.
        let mut merged = writer.segment_metadata().to_vec();
        merged.push(("runtime.main".into(), "0,1,2,3".into()));
        writer.update_segment_metadata(merged);

        // Write enough events to trigger rotation — rotated segments should
        // contain both static and dynamic metadata.
        for _ in 0..4 {
            writer.write_encoded_batch(&test_batch()).unwrap();
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

        // First segment was constructed before update_dynamic_metadata, so
        // it only has static metadata. Rotated segments have both.
        for file in &files[1..] {
            let all_events = format::decode_events(&std::fs::read(file).unwrap()).unwrap();
            let meta: Vec<_> = all_events
                .iter()
                .filter_map(|e| match e {
                    TelemetryEvent::SegmentMetadata { entries, .. } => Some(entries.clone()),
                    _ => None,
                })
                .collect();
            assert_eq!(
                meta.len(),
                1,
                "{}: expected 1 metadata event",
                file.display()
            );
            assert!(
                meta[0].contains(&("service".to_string(), "myapp".to_string())),
                "{}: missing static metadata",
                file.display()
            );
            assert!(
                meta[0].contains(&("runtime.main".to_string(), "0,1,2,3".to_string())),
                "{}: missing dynamic runtime worker metadata",
                file.display()
            );
        }
    }

    #[test]
    fn test_segment_metadata_empty_entries() {
        let dir = TempDir::new().unwrap();
        let path = dir.path().join("trace.bin");
        let mut writer = RotatingWriter::single_file(&path).unwrap();
        writer.write_encoded_batch(&test_batch()).unwrap();
        writer.flush().unwrap();

        let all_events =
            format::decode_events(&std::fs::read(writer.current_active_path()).unwrap()).unwrap();
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

    /// When the background worker has renamed a sealed `.bin` to `.bin.gz`,
    /// eviction should clean up the `.gz` variant instead of silently leaking it.
    #[test]
    fn test_eviction_removes_gz_variant() {
        let dir = TempDir::new().unwrap();
        let base = dir.path().join("trace");
        let one_event = single_event_file_size();
        let max_file_size = one_event;
        // Budget fits many files so segment 0 is not immediately evicted.
        let max_total_size = max_file_size * 100;
        let mut writer = RotatingWriter::new(&base, max_file_size, max_total_size).unwrap();

        // Write two batches: the first fills segment 0, the second triggers
        // rotation (sealing segment 0 as trace.0.bin) and starts segment 1.
        writer.write_encoded_batch(&test_batch()).unwrap();
        writer.write_encoded_batch(&test_batch()).unwrap();
        // Segment 0 is now sealed as trace.0.bin.

        // Simulate the background worker renaming trace.0.bin → trace.0.bin.gz.
        let seg0 = dir.path().join("trace.0.bin");
        let seg0_gz = dir.path().join("trace.0.bin.gz");
        assert!(seg0.exists(), "trace.0.bin should exist after rotation");
        std::fs::rename(&seg0, &seg0_gz).unwrap();

        // Now shrink the budget so the next rotation triggers eviction of
        // segment 0 (which has been renamed to .bin.gz on disk).
        writer.max_total_size = max_file_size;
        for _ in 0..3 {
            writer.write_encoded_batch(&test_batch()).unwrap();
        }
        writer.finalize().unwrap();

        // The .bin.gz file should have been cleaned up by eviction.
        assert!(!seg0_gz.exists(), "trace.0.bin.gz should have been evicted");
    }
}
