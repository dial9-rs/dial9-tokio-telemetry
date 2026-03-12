use crate::telemetry::events::{CpuSampleSource, RawEvent};
#[cfg(test)]
use crate::telemetry::events::TelemetryEvent;
#[allow(unused_imports)] // CallframeDefEvent only used with cpu-profiling
use crate::telemetry::format::{self, PollStartEvent, PollEndEvent, WorkerParkEvent,
    WorkerUnparkEvent, QueueSampleEvent, TaskSpawnEvent, TaskTerminateEvent, WakeEventEvent,
    CpuSampleEvent, CallframeDefEvent, ThreadNameDefEvent};
use dial9_trace_format::encoder::Encoder;
use dial9_trace_format::types::StackFrames;
use dial9_trace_format::InternedString;
use std::collections::{HashMap, HashSet, VecDeque};
use std::fs::{self, File};
use std::io::{self, BufWriter, Write};
use std::panic::Location;
use std::path::{Path, PathBuf};

pub trait TraceWriter: Send {
    /// Write a raw event from the worker thread buffer. The writer handles
    /// interning spawn locations and encoding to the wire format.
    fn write_raw_event(&mut self, raw: RawEvent) -> io::Result<()>;

    /// Write a CPU sample. The writer handles emitting ThreadNameDef/CallframeDef
    /// events and file rotation internally.
    fn write_cpu_sample(
        &mut self,
        timestamp_nanos: u64,
        worker_id: usize,
        tid: u32,
        source: CpuSampleSource,
        callchain: &[u64],
        thread_name: Option<&str>,
    ) -> io::Result<()>;

    /// Enable inline callframe symbolication for CPU samples.
    fn set_inline_callframe_symbols(&mut self, _enabled: bool) {}

    fn flush(&mut self) -> io::Result<()>;
}

impl<W: TraceWriter + ?Sized> TraceWriter for Box<W> {
    fn write_raw_event(&mut self, raw: RawEvent) -> io::Result<()> {
        (**self).write_raw_event(raw)
    }
    fn write_cpu_sample(
        &mut self,
        timestamp_nanos: u64,
        worker_id: usize,
        tid: u32,
        source: CpuSampleSource,
        callchain: &[u64],
        thread_name: Option<&str>,
    ) -> io::Result<()> {
        (**self).write_cpu_sample(timestamp_nanos, worker_id, tid, source, callchain, thread_name)
    }
    fn set_inline_callframe_symbols(&mut self, enabled: bool) {
        (**self).set_inline_callframe_symbols(enabled);
    }
    fn flush(&mut self) -> io::Result<()> {
        (**self).flush()
    }
}

/// A writer that discards all events. Useful for benchmarking hook overhead
/// without I/O costs.
pub struct NullWriter;

impl TraceWriter for NullWriter {
    fn write_raw_event(&mut self, _raw: RawEvent) -> io::Result<()> {
        Ok(())
    }
    fn write_cpu_sample(&mut self, _: u64, _: usize, _: u32, _: CpuSampleSource, _: &[u64], _: Option<&str>) -> io::Result<()> {
        Ok(())
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
/// Owns the encoder and interning state for spawn locations, thread names,
/// and callframe symbols. Handles converting `RawEvent`s and CPU samples
/// to wire format.
#[allow(dead_code)] // callframe fields only used with cpu-profiling feature
pub struct RotatingWriter {
    base_path: PathBuf,
    max_file_size: u64,
    max_total_size: u64,
    files: VecDeque<(PathBuf, u64)>,
    total_size: u64,
    encoder: Encoder<CountingWriter<BufWriter<File>>>,
    next_index: u32,
    stopped: bool,
    has_events_in_current_file: bool,
    locations: SpawnLocationCache,
    // CPU def tracking (absorbed from CpuFlushState)
    inline_callframe_symbols: bool,
    /// Addresses already symbolicated (across all files). Maps addr → (symbol, location).
    callframe_intern: HashMap<u64, (String, Option<String>)>,
    /// Addresses whose CallframeDef has been emitted in the current file.
    callframe_emitted_this_file: HashSet<u64>,
    /// tid → thread name, cached across files.
    thread_name_intern: HashMap<u32, String>,
    /// tids whose ThreadNameDef has been emitted in the current file.
    thread_name_emitted_this_file: HashSet<u32>,
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
            has_events_in_current_file: false,
            locations: SpawnLocationCache::new(),
            inline_callframe_symbols: false,
            callframe_intern: HashMap::new(),
            callframe_emitted_this_file: HashSet::new(),
            thread_name_intern: HashMap::new(),
            thread_name_emitted_this_file: HashSet::new(),
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
            has_events_in_current_file: false,
            locations: SpawnLocationCache::new(),
            inline_callframe_symbols: false,
            callframe_intern: HashMap::new(),
            callframe_emitted_this_file: HashSet::new(),
            thread_name_intern: HashMap::new(),
            thread_name_emitted_this_file: HashSet::new(),
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
        self.has_events_in_current_file = false;
        self.locations.intern_map.clear();
        self.callframe_emitted_this_file.clear();
        self.thread_name_emitted_this_file.clear();

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

    /// Write a CPU sample, emitting any necessary ThreadNameDef/CallframeDef
    /// events first. Handles rotation internally — defs and sample always land
    /// in the same file.
    fn write_cpu_sample_inner(
        &mut self,
        timestamp_nanos: u64,
        worker_id: usize,
        tid: u32,
        source: CpuSampleSource,
        callchain: &[u64],
        thread_name: Option<&str>,
    ) -> io::Result<()> {
        if self.stopped {
            return Ok(());
        }
        self.maybe_rotate()?;
        if self.stopped {
            return Ok(());
        }

        // If we rotated, per-file sets are already cleared — defs will be re-emitted.

        // Cache thread name if new
        if let Some(name) = thread_name {
            self.thread_name_intern.entry(tid).or_insert_with(|| name.to_string());
        }

        // Emit ThreadNameDef if needed for this file
        if !self.thread_name_emitted_this_file.contains(&tid) {
            if let Some(name) = self.thread_name_intern.get(&tid) {
                self.encoder.write(&ThreadNameDefEvent { tid, name: name.clone() })?;
            }
            self.thread_name_emitted_this_file.insert(tid);
        }

        // Emit CallframeDefs if inline symbolication is enabled
        #[cfg(feature = "cpu-profiling")]
        if self.inline_callframe_symbols {
            for &addr in callchain {
                if !self.callframe_emitted_this_file.contains(&addr) {
                    self.callframe_intern.entry(addr).or_insert_with(|| {
                        let sym = dial9_perf_self_profile::resolve_symbol(addr);
                        let symbol = sym.name.unwrap_or_else(|| format!("{:#x}", addr));
                        let location = sym.code_info.map(|info| match info.line {
                            Some(line) => format!("{}:{}", info.file, line),
                            None => info.file,
                        });
                        (symbol, location)
                    });
                    let (symbol, location) = self.callframe_intern[&addr].clone();
                    self.encoder.write(&CallframeDefEvent {
                        address: addr,
                        symbol,
                        location: location.unwrap_or_default(),
                    })?;
                    self.callframe_emitted_this_file.insert(addr);
                }
            }
        }

        // Emit the CpuSample itself
        self.encoder.write(&CpuSampleEvent {
            timestamp_ns: timestamp_nanos,
            worker_id: worker_id as u8,
            tid,
            source,
            callchain: StackFrames(callchain.to_vec()),
        })?;
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

    fn write_cpu_sample(
        &mut self,
        timestamp_nanos: u64,
        worker_id: usize,
        tid: u32,
        source: CpuSampleSource,
        callchain: &[u64],
        thread_name: Option<&str>,
    ) -> io::Result<()> {
        self.write_cpu_sample_inner(timestamp_nanos, worker_id, tid, source, callchain, thread_name)
    }

    fn set_inline_callframe_symbols(&mut self, enabled: bool) {
        self.inline_callframe_symbols = enabled;
    }

    fn flush(&mut self) -> io::Result<()> {
        if !self.stopped {
            self.encoder.flush()?;
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::telemetry::analysis::TraceReader;
    use tempfile::TempDir;

    fn write_park(writer: &mut RotatingWriter) {
        writer.write_raw_event(RawEvent::WorkerPark {
            timestamp_nanos: 1000,
            worker_id: 0,
            worker_local_queue_depth: 2,
            cpu_time_nanos: 0,
        }).unwrap();
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
    fn test_write_raw_event() {
        let dir = TempDir::new().unwrap();
        let path = dir.path().join("test_event_v2.bin");
        let mut writer = RotatingWriter::single_file(&path).unwrap();

        write_park(&mut writer);
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
            write_park(&mut writer);
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
            write_park(&mut writer);
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
            write_park(&mut writer);
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
            write_park(&mut writer);
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
            write_park(&mut writer);
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
            write_park(&mut writer);
        }
        assert!(writer.flush().is_ok());
        assert!(writer.flush().is_ok());
    }

    #[test]
    fn test_write_cpu_sample() {
        let dir = TempDir::new().unwrap();
        let path = dir.path().join("test_cpu.bin");
        let mut writer = RotatingWriter::single_file(&path).unwrap();

        writer.write_cpu_sample(
            1000, 0, 12345, CpuSampleSource::CpuProfile,
            &[0x1000, 0x2000], Some("tokio-runtime-worker"),
        ).unwrap();
        writer.flush().unwrap();

        // read_all filters ThreadNameDef, so we only see CpuSample
        let events = read_trace_events(path.to_str().unwrap());
        assert_eq!(events.len(), 1);
        assert!(matches!(events[0], TelemetryEvent::CpuSample { .. }));

        // read_raw_event sees everything including ThreadNameDef
        let mut reader = TraceReader::new(path.to_str().unwrap()).unwrap();
        let mut all = Vec::new();
        while let Some(ev) = reader.read_raw_event().unwrap() {
            all.push(ev);
        }
        assert_eq!(all.len(), 2);
        assert!(matches!(all[0], TelemetryEvent::ThreadNameDef { .. }));
        assert!(matches!(all[1], TelemetryEvent::CpuSample { .. }));
    }

    #[test]
    fn test_mixed_event_sizes() {
        let dir = TempDir::new().unwrap();
        let base = dir.path().join("trace");
        let mut writer = RotatingWriter::new(&base, 360, 100000).unwrap();

        write_park(&mut writer);
        let loc = std::panic::Location::caller();
        writer.write_raw_event(RawEvent::PollStart {
            timestamp_nanos: 1000,
            worker_id: 0,
            worker_local_queue_depth: 2,
            task_id: crate::telemetry::task_metadata::TaskId::from_u32(1),
            location: loc,
        }).unwrap();
        writer.write_raw_event(RawEvent::WorkerUnpark {
            timestamp_nanos: 1000,
            worker_id: 0,
            worker_local_queue_depth: 2,
            cpu_time_nanos: 0,
            sched_wait_delta_nanos: 0,
        }).unwrap();
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
