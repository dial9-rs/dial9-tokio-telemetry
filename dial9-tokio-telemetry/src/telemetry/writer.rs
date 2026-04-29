use dial9_trace_format::encoder::{Encoder, RawEncoder};

use crate::rate_limit::rate_limited;
use crate::telemetry::collector::Batch;
use crate::telemetry::events::clock_pair;
use crate::telemetry::format::{ClockSyncEvent, SegmentMetadataEvent};
use std::collections::VecDeque;
use std::fs::{self, File};
use std::io::BufWriter;
use std::path::{Path, PathBuf};
use std::time::{Duration, SystemTime};

use metrique_timesource::time_source;

/// Trait for writing encoded telemetry batches to a destination.
pub trait TraceWriter: Send {
    /// Flush buffered data to the underlying storage.
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
    /// Merge the segment metadata entries that will be written into the next
    /// rotated segment (e.g. merged static + runtime names). Default is a no-op.
    fn update_segment_metadata(&mut self, _entries: Vec<(String, String)>) {}
    /// Write a `SegmentMetadataEvent` into the current segment. Called before
    /// finalize so that single-segment traces contain runtime→worker mappings.
    /// Default is a no-op.
    fn write_current_segment_metadata(&mut self) -> std::io::Result<()> {
        Ok(())
    }
    /// Returns `true` when the flush loop should drain all thread-local
    /// buffers before the next step. For `RotatingWriter` this fires when a
    /// rotation boundary is crossed *or* when a periodic drain interval
    /// elapses (whichever comes first). Default returns `false`.
    fn should_drain(&self) -> bool {
        false
    }
    /// Called by the flush loop after thread-local buffers have been drained
    /// and flushed. The writer may rotate the segment, advance a drain timer,
    /// or do nothing. Returns `true` if a segment rotation occurred.
    fn drained(&mut self) -> std::io::Result<bool> {
        Ok(false)
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
    fn should_drain(&self) -> bool {
        (**self).should_drain()
    }
    fn drained(&mut self) -> std::io::Result<bool> {
        (**self).drained()
    }
}

#[derive(Default, Clone)]
struct SegmentMetadata {
    entries: Vec<(String, String)>,
}

impl SegmentMetadata {
    fn new(entries: Vec<(String, String)>) -> Self {
        Self { entries }
    }

    /// Merge incoming entries with existing ones. Incoming entries take priority
    /// on key conflict; existing entries with keys not in the incoming set are preserved.
    /// Returns `true` if the resulting entries differ from the previous state.
    fn merge(&mut self, entries: impl Iterator<Item = (String, String)>) -> bool {
        let mut merged: Vec<(String, String)> = entries.collect();
        for (k, v) in &self.entries {
            if !merged.iter().any(|(mk, _)| mk == k) {
                merged.push((k.clone(), v.clone()));
            }
        }
        if merged == self.entries {
            return false;
        }
        self.entries = merged;
        true
    }
}

/// A writer that discards all events. Useful for benchmarking hook overhead
/// without I/O costs.
#[derive(Debug)]
pub struct NullWriter;

impl TraceWriter for NullWriter {
    fn write_encoded_batch(&mut self, _batch: &Batch) -> std::io::Result<()> {
        Ok(())
    }
    fn flush(&mut self) -> std::io::Result<()> {
        Ok(())
    }
}

/// Default rotation period: 1 minute.
const DEFAULT_ROTATION_PERIOD: Duration = Duration::from_secs(60);

/// Default maximum interval between thread-local buffer drains.
const DEFAULT_DRAIN_INTERVAL: Duration = Duration::from_secs(30);

/// A writer that rotates trace files to bound disk usage and time.
///
/// Rotation triggers when *either* condition is met:
/// - `max_file_size`: the current file exceeds this many bytes
/// - `rotation_period`: a wall-clock-aligned time boundary is crossed
///   (default: 1 minute, aligned to round minute boundaries)
///
/// **Prefer time-based rotation.** Time-based rotation is coordinated with the
/// flush loop: thread-local buffers are drained before the segment is sealed,
/// so each segment contains events from a clean, non-overlapping time window.
/// Size-based rotation fires immediately when the threshold is crossed and does
/// not drain thread-local buffers, so segments may contain events that overlap
/// in time. Set `max_file_size` large enough that time-based rotation fires
/// first under normal conditions (e.g. 100 MB or more). Size-based rotation
/// then acts as a safety valve for unexpected data bursts.
///
/// `max_total_size` controls eviction: oldest files are deleted when total
/// size across all files exceeds this budget.
///
/// Files are named `{base_path}.0.bin`, `{base_path}.1.bin`, etc.
/// Each file is a self-contained trace with its own header.
pub struct RotatingWriter {
    base_path: PathBuf,
    max_file_size: u64,
    max_total_size: u64,
    /// How often to rotate based on wall-clock time. `Duration::MAX` disables
    /// time-based rotation (used by `single_file()`).
    rotation_period: Duration,
    /// The next wall-clock instant at which time-based rotation should fire.
    next_rotation_time: SystemTime,
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
    segment_metadata: SegmentMetadata,
    /// Events silently dropped because the writer was finished/stopped.
    dropped_events: usize,
    /// Whether any real (non-metadata) events have been written to the current segment.
    /// Reset on rotation; used by `finalize()` to avoid sealing empty segments.
    has_real_events: bool,
    /// How often the flush loop should drain thread-local buffers, independent
    /// of rotation. Defaults to `min(rotation_period, 30s)`.
    drain_interval: Duration,
    /// Next wall-clock instant at which `should_drain()` returns true.
    next_drain_time: SystemTime,
}

impl std::fmt::Debug for RotatingWriter {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("RotatingWriter")
            .field("base_path", &self.base_path)
            .field("max_file_size", &self.max_file_size)
            .field("max_total_size", &self.max_total_size)
            .finish_non_exhaustive()
    }
}

// the write side is obviously marge larger than the `Finished` size so clippy warns on this
// but we don't want to force going through a pointer every time we want to write.
#[allow(clippy::large_enum_variant)]
enum WriterState {
    /// Writer is open and events can be written
    Active {
        writer: RawEncoder<BufWriter<File>>,
        need_metadata: bool,
    },

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
        Self::create(
            base_path,
            max_file_size,
            max_total_size,
            DEFAULT_ROTATION_PERIOD,
            SegmentMetadata::default(),
        )
    }

    /// Create a `RotatingWriterBuilder` for advanced configuration.
    #[builder(builder_type = RotatingWriterBuilder, finish_fn = build)]
    pub fn builder(
        base_path: impl Into<PathBuf>,
        max_file_size: u64,
        max_total_size: u64,
        /// How often to rotate based on wall-clock time, aligned to round
        /// boundaries (e.g. a 60 s period rotates at the top of each minute).
        /// Defaults to 60 seconds.
        rotation_period: Option<Duration>,
        segment_metadata: Option<Vec<(String, String)>>,
    ) -> std::io::Result<Self> {
        Self::create(
            base_path,
            max_file_size,
            max_total_size,
            rotation_period.unwrap_or(DEFAULT_ROTATION_PERIOD),
            segment_metadata
                .map(SegmentMetadata::new)
                .unwrap_or_default(),
        )
    }

    fn create(
        base_path: impl Into<PathBuf>,
        max_file_size: u64,
        max_total_size: u64,
        rotation_period: Duration,
        segment_metadata: SegmentMetadata,
    ) -> std::io::Result<Self> {
        if rotation_period == Duration::from_secs(0) {
            return Err(std::io::Error::other("Rotation period must not be zero"));
        }
        let base_path = base_path.into();
        if let Some(parent) = base_path.parent() {
            fs::create_dir_all(parent)?;
        }
        let first_path = Self::active_path(&base_path, 0);
        let file = File::create(&first_path)?;
        let writer = BufWriter::new(file);
        let state = Self::prepare_segment(writer)?;
        let now = time_source().system_time().as_std();
        let drain_interval = rotation_period.min(DEFAULT_DRAIN_INTERVAL);

        Ok(Self {
            base_path,
            max_file_size,
            max_total_size,
            rotation_period,
            next_rotation_time: Self::next_boundary(now, rotation_period),
            closed_files: VecDeque::new(),
            active_path: first_path,
            state,
            next_index: 1,
            did_rotate: false,
            segment_metadata,
            dropped_events: 0,
            has_real_events: false,
            drain_interval,
            next_drain_time: Self::next_boundary(now, drain_interval),
        })
    }

    /// Create a writer that writes to a single file with no rotation or eviction.
    /// The segment is written to `{stem}.0.bin.active` while active, then sealed
    /// to `{stem}.0.bin` on `finalize`. The background worker will symbolize
    /// and gzip it to `{stem}.0.bin.gz`.
    ///
    /// Note: This API does not allow the ability to provide custom segment metadata.
    /// Time-based rotation is disabled.
    pub fn single_file(path: impl Into<PathBuf>) -> std::io::Result<Self> {
        let path = path.into();
        if let Some(parent) = path.parent() {
            fs::create_dir_all(parent)?;
        }
        let active_path = Self::active_path(&path, 0);
        let file = File::create(&active_path)?;
        let writer = BufWriter::new(file);
        let state = Self::prepare_segment(writer)?;
        let now = time_source().system_time().as_std();

        Ok(Self {
            base_path: path,
            max_file_size: u64::MAX,
            max_total_size: u64::MAX,
            rotation_period: Duration::MAX,
            next_rotation_time: Self::next_boundary(now, Duration::MAX),
            closed_files: VecDeque::new(),
            active_path,
            state,
            next_index: 1,
            did_rotate: false,
            segment_metadata: SegmentMetadata::default(),
            dropped_events: 0,
            has_real_events: false,
            drain_interval: DEFAULT_DRAIN_INTERVAL,
            next_drain_time: Self::next_boundary(now, DEFAULT_DRAIN_INTERVAL),
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

    /// Create an encoder, write the file header, segment metadata, and a
    /// clock-sync anchor, then convert to a [`RawEncoder`] for the
    /// remainder of the file's lifetime.
    fn prepare_segment(writer: BufWriter<File>) -> std::io::Result<WriterState> {
        let mut encoder = Encoder::new_to(writer)?;
        let (mono, real) = clock_pair();
        encoder.write(&ClockSyncEvent {
            timestamp_ns: mono,
            realtime_ns: real,
        })?;
        Ok(WriterState::Active {
            writer: encoder.into_raw_encoder(),
            need_metadata: true,
        })
    }

    fn write_metadata_if_needed(&mut self) -> std::io::Result<()> {
        match &mut self.state {
            WriterState::Active {
                writer,
                need_metadata,
            } => {
                if *need_metadata {
                    Self::write_segment_metadata(writer, &self.segment_metadata.entries)?;
                }
                *need_metadata = false;
                Ok(())
            }
            WriterState::Finished => Ok(()),
        }
    }

    /// Write a `SegmentMetadataEvent` and a fresh `ClockSyncEvent` into
    /// the current active segment.
    fn write_segment_metadata(
        writer: &mut RawEncoder<BufWriter<File>>,
        entries: &[(String, String)],
    ) -> std::io::Result<()> {
        let mut enc = Encoder::new();
        let entries = entries.to_vec();
        let (mono, real) = clock_pair();
        enc.write(&SegmentMetadataEvent {
            timestamp_ns: mono,
            entries,
        })?;
        enc.write(&ClockSyncEvent {
            timestamp_ns: mono,
            realtime_ns: real,
        })?;
        writer.write_raw(&enc.finish())?;
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

    /// Compute the next wall-clock-aligned rotation boundary after `now`.
    ///
    /// For a 60 s period, if `now` is 14:03:22 the result is 14:04:00.
    /// Returns a far-future time when `period` is `Duration::MAX` (time
    /// rotation disabled).
    fn next_boundary(now: SystemTime, period: Duration) -> SystemTime {
        if period == Duration::MAX {
            // ~year 2554 — far enough to never trigger, small enough to not overflow.
            return SystemTime::UNIX_EPOCH + Duration::from_secs(u32::MAX as u64 * 4);
        }
        let epoch_dur = now
            .duration_since(SystemTime::UNIX_EPOCH)
            .unwrap_or_default();
        let period_nanos = period.as_nanos();
        if period_nanos == 0 {
            return now;
        }
        let epoch_nanos = epoch_dur.as_nanos();
        let next_nanos = ((epoch_nanos / period_nanos) + 1) * period_nanos;
        SystemTime::UNIX_EPOCH + Duration::from_nanos(next_nanos as u64)
    }

    fn rotate(&mut self) -> std::io::Result<()> {
        let WriterState::Active { writer: raw, .. } = &mut self.state else {
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
        self.state = Self::prepare_segment(writer)?;
        self.active_path = new_path;
        self.did_rotate = true;
        self.has_real_events = false;
        let now = time_source().system_time().as_std();
        self.next_rotation_time = Self::next_boundary(now, self.rotation_period);
        self.next_drain_time = Self::next_boundary(now, self.drain_interval);

        tracing::debug!(
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
            WriterState::Active { writer, .. } => writer.bytes_written(),
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
                                    rate_limited!(Duration::from_secs(60), {
                                        tracing::warn!(
                                            "failed to evict old trace segment {}: {e2}",
                                            entry.path().display()
                                        );
                                    });
                                }
                            }
                        }
                    }
                    Err(e) => {
                        rate_limited!(Duration::from_secs(60), {
                            tracing::warn!(
                                "failed to evict old trace segment {}: {e}",
                                path.display()
                            );
                        });
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
        let WriterState::Active { writer: raw, .. } = &self.state else {
            return Ok(());
        };
        if raw.bytes_written() > self.max_file_size {
            self.rotate()?;
        }
        Ok(())
    }
}

impl TraceWriter for RotatingWriter {
    fn flush(&mut self) -> std::io::Result<()> {
        if let WriterState::Active { writer: raw, .. } = &mut self.state {
            raw.flush()?;
        }
        Ok(())
    }

    fn take_rotated(&mut self) -> bool {
        std::mem::take(&mut self.did_rotate)
    }

    fn segment_metadata(&self) -> &[(String, String)] {
        &self.segment_metadata.entries
    }

    fn update_segment_metadata(&mut self, entries: Vec<(String, String)>) {
        if self.segment_metadata.merge(entries.into_iter()) {
            match &mut self.state {
                WriterState::Active { need_metadata, .. } => *need_metadata = true,
                WriterState::Finished => {}
            }
        }
    }

    fn write_current_segment_metadata(&mut self) -> std::io::Result<()> {
        self.write_metadata_if_needed()
    }

    fn should_drain(&self) -> bool {
        self.has_real_events && time_source().system_time() >= self.next_drain_time
    }

    fn drained(&mut self) -> std::io::Result<bool> {
        if !self.has_real_events {
            return Ok(false);
        }
        let now = time_source().system_time();
        if now >= self.next_rotation_time {
            self.rotate()?;
            return Ok(true);
        }
        // Periodic drain without rotation — just advance the drain timer.
        self.next_drain_time = Self::next_boundary(now.as_std(), self.drain_interval);
        Ok(false)
    }

    fn finalize(&mut self) -> std::io::Result<()> {
        if matches!(self.state, WriterState::Finished) {
            rate_limited!(Duration::from_secs(60), {
                tracing::warn!("writer is already closed.");
            });
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
        self.write_metadata_if_needed()?;
        let WriterState::Active { writer: raw, .. } = &mut self.state else {
            self.dropped_events += batch.event_count as usize;
            return Ok(());
        };
        if batch.event_count > 0 {
            // Note: we do NOT advance next_rotation_time or next_drain_time
            // when the first event arrives in an empty segment, even if the
            // timers are stale. The drain state machine (Idle → EpochBumped →
            // drain) takes 3 flush cycles (~15ms) to complete, so by the time
            // drained() is called there will be multiple batches in the segment,
            // not a single event. Advancing the timers here would skip rotation
            // windows and produce fewer segments than expected.
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
            rate_limited!(Duration::from_secs(60), {
                tracing::info!(
                    target: "dial9_telemetry",
                    dropped_events = self.dropped_events,
                    "RotatingWriter dropped events after finalization"
                );
            });
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
            .filter(|e| {
                !matches!(
                    e,
                    TelemetryEvent::SegmentMetadata { .. } | TelemetryEvent::ClockSync { .. }
                )
            })
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

    // ---- Time-based rotation tests ----

    #[test]
    fn test_next_boundary_aligns_to_minute() {
        use std::time::{Duration, SystemTime};
        // 2026-01-01 14:03:22 UTC → epoch 1767272602
        let now = SystemTime::UNIX_EPOCH + Duration::from_secs(1_767_272_602);
        let period = Duration::from_secs(60);
        let boundary = RotatingWriter::next_boundary(now, period);
        // Should align to 14:04:00 → epoch 1767272640
        let expected = SystemTime::UNIX_EPOCH + Duration::from_secs(1_767_272_640);
        assert_eq!(boundary, expected);
    }

    #[test]
    fn test_next_boundary_at_exact_boundary() {
        use std::time::{Duration, SystemTime};
        // Exactly on a minute boundary: 14:03:00 → epoch 1767272580
        let now = SystemTime::UNIX_EPOCH + Duration::from_secs(1_767_272_580);
        let period = Duration::from_secs(60);
        let boundary = RotatingWriter::next_boundary(now, period);
        // Should advance to 14:04:00
        let expected = SystemTime::UNIX_EPOCH + Duration::from_secs(1_767_272_640);
        assert_eq!(boundary, expected);
    }

    #[test]
    fn test_next_boundary_5_minute_alignment() {
        use std::time::{Duration, SystemTime};
        // 14:03:22 with 5-minute period
        let now = SystemTime::UNIX_EPOCH + Duration::from_secs(1_767_272_602);
        let period = Duration::from_secs(300);
        let boundary = RotatingWriter::next_boundary(now, period);
        // Should align to 14:05:00 → epoch 1767272700
        let expected = SystemTime::UNIX_EPOCH + Duration::from_secs(1_767_272_700);
        assert_eq!(boundary, expected);
    }

    #[test]
    fn test_next_boundary_duration_max_returns_far_future() {
        use std::time::{Duration, SystemTime};
        let now = SystemTime::now();
        let boundary = RotatingWriter::next_boundary(now, Duration::MAX);
        // Should be far in the future — never triggers
        assert!(boundary > now + Duration::from_secs(86400 * 365 * 100));
    }

    #[tokio::test(start_paused = true)]
    async fn test_time_rotation_triggers_on_expired_boundary() {
        use metrique_timesource::{TimeSource, tokio::set_time_source_for_current_runtime};
        let _guard = set_time_source_for_current_runtime(TimeSource::tokio(std::time::UNIX_EPOCH));

        let dir = TempDir::new().unwrap();
        let base = dir.path().join("trace");
        let mut writer = RotatingWriter::builder()
            .base_path(&base)
            .max_file_size(u64::MAX)
            .max_total_size(100_000)
            .rotation_period(Duration::from_secs(60))
            .build()
            .unwrap();

        writer.write_encoded_batch(&test_batch()).unwrap();
        writer.flush().unwrap();
        let initial_index = writer.next_index;

        // Advance past the 60s boundary
        tokio::time::advance(Duration::from_secs(61)).await;

        // Time-based rotation is now driven by drained(), not write_encoded_batch.
        writer.write_encoded_batch(&test_batch()).unwrap();
        writer.flush().unwrap();
        writer.drained().unwrap();

        assert!(
            writer.next_index > initial_index,
            "expected time-based rotation to trigger"
        );
        writer.finalize().unwrap();

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

    #[tokio::test(start_paused = true)]
    async fn test_time_rotation_skips_when_no_real_events() {
        use metrique_timesource::{TimeSource, tokio::set_time_source_for_current_runtime};
        let _guard = set_time_source_for_current_runtime(TimeSource::tokio(std::time::UNIX_EPOCH));

        let dir = TempDir::new().unwrap();
        let base = dir.path().join("trace");
        let mut writer = RotatingWriter::builder()
            .base_path(&base)
            .max_file_size(u64::MAX)
            .max_total_size(100_000)
            .rotation_period(Duration::from_secs(60))
            .build()
            .unwrap();

        // Advance past the boundary without writing any events
        tokio::time::advance(Duration::from_secs(120)).await;

        let empty_batch = Batch {
            encoded_bytes: vec![],
            event_count: 0,
        };
        writer.write_encoded_batch(&empty_batch).unwrap();

        assert_eq!(
            writer.next_index, 1,
            "should not rotate when no real events exist"
        );
        writer.finalize().unwrap();
    }

    #[test]
    fn test_size_rotation_still_works_with_time_disabled() {
        let dir = TempDir::new().unwrap();
        let base = dir.path().join("trace");
        let one_event = single_event_file_size();
        let mut writer = RotatingWriter::builder()
            .base_path(&base)
            .max_file_size(one_event)
            .max_total_size(100_000)
            .rotation_period(std::time::Duration::MAX)
            .build()
            .unwrap();

        for _ in 0..3 {
            writer.write_encoded_batch(&test_batch()).unwrap();
        }
        writer.finalize().unwrap();

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

    #[tokio::test(start_paused = true)]
    async fn test_time_rotation_respects_eviction_budget() {
        use metrique_timesource::{TimeSource, tokio::set_time_source_for_current_runtime};
        let _guard = set_time_source_for_current_runtime(TimeSource::tokio(std::time::UNIX_EPOCH));

        let dir = TempDir::new().unwrap();
        let base = dir.path().join("trace");
        let one_event = single_event_file_size();
        let mut writer = RotatingWriter::builder()
            .base_path(&base)
            .max_file_size(u64::MAX)
            .max_total_size(one_event * 3)
            .rotation_period(Duration::from_secs(60))
            .build()
            .unwrap();

        writer.write_encoded_batch(&test_batch()).unwrap();
        for _ in 0..5 {
            tokio::time::advance(Duration::from_secs(61)).await;
            writer.write_encoded_batch(&test_batch()).unwrap();
            writer.drained().unwrap();
        }
        writer.finalize().unwrap();

        assert!(
            total_disk_usage(dir.path()) <= one_event * 3,
            "disk usage should stay within budget"
        );
    }

    #[test]
    fn test_builder_rotation_period_default() {
        let dir = TempDir::new().unwrap();
        let base = dir.path().join("trace");
        let writer = RotatingWriter::builder()
            .base_path(&base)
            .max_file_size(1024)
            .max_total_size(100_000)
            .build()
            .unwrap();
        assert_eq!(writer.rotation_period, DEFAULT_ROTATION_PERIOD);
    }

    #[test]
    fn test_new_uses_default_rotation_period() {
        let dir = TempDir::new().unwrap();
        let base = dir.path().join("trace");
        let writer = RotatingWriter::new(&base, 1024, 100_000).unwrap();
        assert_eq!(writer.rotation_period, DEFAULT_ROTATION_PERIOD);
    }

    #[tokio::test(start_paused = true)]
    async fn test_finalize_after_time_rotation() {
        use metrique_timesource::{TimeSource, tokio::set_time_source_for_current_runtime};
        let _guard = set_time_source_for_current_runtime(TimeSource::tokio(std::time::UNIX_EPOCH));

        let dir = TempDir::new().unwrap();
        let base = dir.path().join("trace");
        let mut writer = RotatingWriter::builder()
            .base_path(&base)
            .max_file_size(u64::MAX)
            .max_total_size(100_000)
            .rotation_period(Duration::from_secs(60))
            .build()
            .unwrap();

        writer.write_encoded_batch(&test_batch()).unwrap();
        tokio::time::advance(Duration::from_secs(61)).await;
        writer.write_encoded_batch(&test_batch()).unwrap();
        writer.drained().unwrap();
        writer.finalize().unwrap();

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

    #[tokio::test(start_paused = true)]
    async fn test_stale_boundary_does_not_rotate_first_event() {
        use metrique_timesource::{TimeSource, tokio::set_time_source_for_current_runtime};
        let _guard = set_time_source_for_current_runtime(TimeSource::tokio(std::time::UNIX_EPOCH));

        let dir = TempDir::new().unwrap();
        let base = dir.path().join("trace");
        let mut writer = RotatingWriter::builder()
            .base_path(&base)
            .max_file_size(u64::MAX)
            .max_total_size(100_000)
            .rotation_period(Duration::from_secs(60))
            .build()
            .unwrap();

        // Advance well past the boundary with no events
        tokio::time::advance(Duration::from_secs(300)).await;

        // First event after the gap — should NOT trigger rotation
        writer.write_encoded_batch(&test_batch()).unwrap();
        assert_eq!(
            writer.next_index, 1,
            "first event after idle gap should not trigger immediate rotation"
        );

        // Second event shortly after — still within the new boundary
        writer.write_encoded_batch(&test_batch()).unwrap();
        assert_eq!(
            writer.next_index, 1,
            "second event should still be in the same segment"
        );

        writer.finalize().unwrap();

        let events = read_trace_events(&rotating_file(&base, 0));
        assert_eq!(events.len(), 2, "both events should be in segment 0");
    }

    #[test]
    fn test_clock_sync_precedes_first_data_event() {
        use crate::background_task::sealed::LEGACY_EPOCH_NS_FLOOR;

        let dir = TempDir::new().unwrap();
        let base = dir.path().join("trace");
        let mut writer = RotatingWriter::builder()
            .base_path(&base)
            .max_file_size(100_000)
            .max_total_size(100_000)
            .build()
            .unwrap();
        writer.write_encoded_batch(&test_batch()).unwrap();
        writer.flush().unwrap();
        writer.finalize().unwrap();

        let data = std::fs::read(rotating_file(&base, 0)).unwrap();
        let all = format::decode_events(&data).unwrap();

        // ClockSync must precede the first data event so a streaming
        // decoder never sees a data timestamp without an anchor.
        let first_data_idx = all
            .iter()
            .position(|e| {
                !matches!(
                    e,
                    TelemetryEvent::SegmentMetadata { .. } | TelemetryEvent::ClockSync { .. }
                )
            })
            .expect("expected at least one data event");
        let first_clock_sync_idx = all
            .iter()
            .position(|e| matches!(e, TelemetryEvent::ClockSync { .. }))
            .expect("expected a ClockSyncEvent in the file");
        assert!(first_clock_sync_idx < first_data_idx);

        match &all[first_clock_sync_idx] {
            TelemetryEvent::ClockSync { realtime_nanos, .. } => {
                assert!(*realtime_nanos >= LEGACY_EPOCH_NS_FLOOR);
            }
            _ => unreachable!(),
        }
    }

    #[test]
    fn test_segment_metadata_timestamp_is_monotonic_scale() {
        use crate::background_task::sealed::LEGACY_EPOCH_NS_FLOOR;

        let dir = TempDir::new().unwrap();
        let base = dir.path().join("trace");
        let mut writer = RotatingWriter::builder()
            .base_path(&base)
            .max_file_size(100_000)
            .max_total_size(100_000)
            .build()
            .unwrap();
        writer.write_encoded_batch(&test_batch()).unwrap();
        writer.flush().unwrap();
        writer.finalize().unwrap();

        let data = std::fs::read(rotating_file(&base, 0)).unwrap();
        let all = format::decode_events(&data).unwrap();

        // SegmentMetadata.timestamp_ns should remain monotonic-scale,
        // not epoch wall-clock.
        let seg_ts = all
            .iter()
            .find_map(|e| match e {
                TelemetryEvent::SegmentMetadata {
                    timestamp_nanos, ..
                } => Some(*timestamp_nanos),
                _ => None,
            })
            .expect("SegmentMetadata");
        assert!(
            seg_ts < LEGACY_EPOCH_NS_FLOOR,
            "SegmentMetadata.timestamp_nanos ({seg_ts}) should be monotonic-scale"
        );
    }

    #[test]
    fn test_clock_sync_written_in_every_rotated_file() {
        let dir = TempDir::new().unwrap();
        let base = dir.path().join("trace");
        let one_event = single_event_file_size();
        let mut writer = RotatingWriter::builder()
            .base_path(&base)
            .max_file_size(one_event)
            .max_total_size(100_000)
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
            let all = format::decode_events(&std::fs::read(file).unwrap()).unwrap();
            let has_clock_sync = all
                .iter()
                .any(|e| matches!(e, TelemetryEvent::ClockSync { .. }));
            assert!(
                has_clock_sync,
                "{}: expected ClockSyncEvent",
                file.display()
            );
        }
    }

    /// A hand-built legacy-shaped buffer (SegmentMetadata + WorkerPark,
    /// no ClockSyncEvent) must still round-trip through the decoder.
    #[test]
    fn test_legacy_trace_without_clock_sync_still_decodes() {
        let mut enc = Encoder::new_to(Vec::new()).unwrap();
        enc.write(&SegmentMetadataEvent {
            timestamp_ns: 1,
            entries: vec![("k".into(), "v".into())],
        })
        .unwrap();
        enc.write_infallible(&WorkerParkEvent {
            timestamp_ns: 1000,
            worker_id: crate::telemetry::format::WorkerId::from(0usize),
            local_queue: 0,
            cpu_time_ns: 0,
        });
        let buf = enc.into_inner();

        let all = format::decode_events(&buf).unwrap();
        assert!(
            all.iter()
                .any(|e| matches!(e, TelemetryEvent::WorkerPark { .. })),
            "expected WorkerPark to decode"
        );
        assert!(
            !all.iter()
                .any(|e| matches!(e, TelemetryEvent::ClockSync { .. })),
            "legacy trace must not contain ClockSync"
        );
    }

    #[test]
    fn test_clock_sync_offset_recovers_wall_clock_for_recent_event() {
        use std::time::{SystemTime, UNIX_EPOCH};

        let dir = TempDir::new().unwrap();
        let base = dir.path().join("trace");
        let mut writer = RotatingWriter::builder()
            .base_path(&base)
            .max_file_size(100_000)
            .max_total_size(100_000)
            .build()
            .unwrap();

        // Use a real monotonic reading so reconstruction lands near now.
        let park_ts = crate::telemetry::events::clock_monotonic_ns();
        let mut enc = Encoder::new_to(Vec::new()).unwrap();
        enc.write_infallible(&WorkerParkEvent {
            timestamp_ns: park_ts,
            worker_id: crate::telemetry::format::WorkerId::from(0usize),
            local_queue: 0,
            cpu_time_ns: 0,
        });
        writer
            .write_encoded_batch(&Batch {
                encoded_bytes: enc.into_inner(),
                event_count: 1,
            })
            .unwrap();
        writer.flush().unwrap();
        writer.finalize().unwrap();

        let all = format::decode_events(&std::fs::read(rotating_file(&base, 0)).unwrap()).unwrap();

        let (sync_mono, sync_real) = all
            .iter()
            .find_map(|e| match e {
                TelemetryEvent::ClockSync {
                    timestamp_nanos,
                    realtime_nanos,
                } => Some((*timestamp_nanos, *realtime_nanos)),
                _ => None,
            })
            .expect("ClockSync");
        let park_from_file = all
            .iter()
            .find_map(|e| match e {
                TelemetryEvent::WorkerPark {
                    timestamp_nanos, ..
                } => Some(*timestamp_nanos),
                _ => None,
            })
            .expect("WorkerPark");

        let offset = sync_real as i128 - sync_mono as i128;
        let reconstructed_wall_ns = park_from_file as i128 + offset;
        let now_ns = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_nanos() as i128;
        let diff = (reconstructed_wall_ns - now_ns).abs();
        assert!(
            diff < 5_000_000_000,
            "reconstructed wall clock {reconstructed_wall_ns} diverges from now {now_ns} by {diff}ns"
        );
    }

    /// S3-style metadata set via `update_segment_metadata` before any events
    /// are written must appear in the segment's SegmentMetadata event.
    #[test]
    fn test_update_segment_metadata_appears_in_trace() {
        let dir = TempDir::new().unwrap();
        let base = dir.path().join("trace");
        let mut writer = RotatingWriter::new(&base, 100_000, 100_000).unwrap();

        // Simulate TelemetryCore::new setting S3 metadata
        writer.update_segment_metadata(vec![
            ("bucket".into(), "my-bucket".into()),
            ("service_name".into(), "my-svc".into()),
        ]);

        writer.write_encoded_batch(&test_batch()).unwrap();
        writer.flush().unwrap();
        writer.finalize().unwrap();

        let all = format::decode_events(&std::fs::read(rotating_file(&base, 0)).unwrap()).unwrap();
        let metadata: Vec<_> = all
            .iter()
            .filter_map(|e| match e {
                TelemetryEvent::SegmentMetadata { entries, .. } => Some(entries.clone()),
                _ => None,
            })
            .collect();
        assert!(!metadata.is_empty(), "expected SegmentMetadata event");
        assert!(
            metadata
                .last()
                .unwrap()
                .contains(&("bucket".to_string(), "my-bucket".to_string())),
            "S3 metadata should be in segment"
        );
        assert!(
            metadata
                .last()
                .unwrap()
                .contains(&("service_name".to_string(), "my-svc".to_string())),
            "S3 metadata should be in segment"
        );
    }

    /// Simulates the flush loop pattern: S3 metadata is set once, then
    /// runtime entries are merged repeatedly. S3 metadata must survive.
    #[test]
    fn test_merge_preserves_s3_metadata_across_runtime_updates() {
        let dir = TempDir::new().unwrap();
        let base = dir.path().join("trace");
        let one_event = single_event_file_size();
        let mut writer = RotatingWriter::new(&base, one_event, 100_000).unwrap();

        // Step 1: S3 metadata set (like TelemetryCore::new)
        writer.update_segment_metadata(vec![
            ("bucket".into(), "my-bucket".into()),
            ("service_name".into(), "my-svc".into()),
        ]);

        // Step 2: flush loop merges only runtime entries — S3 metadata
        // set in step 1 must be preserved by the merge logic.
        writer.update_segment_metadata(vec![("runtime.main".into(), "0,1".into())]);

        // Write enough to trigger rotation
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
        assert!(files.len() >= 2, "expected rotation");

        // Rotated segments should contain both S3 and runtime metadata
        for file in &files[1..] {
            let all = format::decode_events(&std::fs::read(file).unwrap()).unwrap();
            let meta: Vec<_> = all
                .iter()
                .filter_map(|e| match e {
                    TelemetryEvent::SegmentMetadata { entries, .. } => Some(entries.clone()),
                    _ => None,
                })
                .collect();
            let last = meta.last().expect("expected SegmentMetadata");
            assert!(
                last.contains(&("bucket".to_string(), "my-bucket".to_string())),
                "{}: S3 metadata lost after merge",
                file.display()
            );
            assert!(
                last.contains(&("runtime.main".to_string(), "0,1".to_string())),
                "{}: runtime metadata missing",
                file.display()
            );
        }
    }

    /// Repeated calls to `update_segment_metadata` with identical entries
    /// should not set `need_metadata`, avoiding redundant writes.
    #[test]
    fn test_update_segment_metadata_no_op_when_unchanged() {
        let dir = TempDir::new().unwrap();
        let base = dir.path().join("trace");
        let mut writer = RotatingWriter::new(&base, 100_000, 100_000).unwrap();

        let entries = vec![("k".into(), "v".into())];
        writer.update_segment_metadata(entries.clone());
        // First batch writes metadata
        writer.write_encoded_batch(&test_batch()).unwrap();

        // Same entries again — should be a no-op
        writer.update_segment_metadata(entries.clone());
        // Second batch should NOT write another metadata event
        writer.write_encoded_batch(&test_batch()).unwrap();
        writer.flush().unwrap();
        writer.finalize().unwrap();

        let all = format::decode_events(&std::fs::read(rotating_file(&base, 0)).unwrap()).unwrap();
        let metadata_count = all
            .iter()
            .filter(|e| matches!(e, TelemetryEvent::SegmentMetadata { .. }))
            .count();
        assert_eq!(
            metadata_count, 1,
            "identical update_segment_metadata should not trigger another write"
        );
    }
}
