mod cpu_flush_state;
mod event_writer;
mod flush_state;
mod shared_state;

pub(crate) use shared_state::SharedState;

use event_writer::EventWriter;

#[cfg(feature = "cpu-profiling")]
use cpu_flush_state::CpuFlushState;

use crate::telemetry::events::RawEvent;
use crate::telemetry::task_metadata::TaskId;
use crate::telemetry::writer::{TraceWriter, WriteAtomicResult};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};
use tokio::runtime::Handle;

pub struct TelemetryRecorder {
    shared: Arc<SharedState>,
    event_writer: EventWriter,
}

impl TelemetryRecorder {
    pub fn new(writer: Box<dyn TraceWriter>) -> Self {
        Self {
            shared: Arc::new(SharedState::new(Instant::now())),
            event_writer: EventWriter::new(writer),
        }
    }

    pub fn initialize(&mut self, handle: Handle) {
        let metrics = handle.metrics();
        self.shared.metrics.store(Arc::new(Some(metrics)));
    }

    fn flush(&mut self) {
        #[cfg(feature = "cpu-profiling")]
        self.event_writer.flush_cpu(&self.shared);

        for batch in self.shared.collector.drain() {
            for raw in batch {
                if let Ok(WriteAtomicResult::Rotated) = self.event_writer.write_raw_event(raw) {
                    self.shared.enabled.store(false, Ordering::Relaxed);
                    return;
                }
            }
        }
        self.event_writer.flush().unwrap();
    }

    pub(crate) fn install(
        builder: &mut tokio::runtime::Builder,
        writer: Box<dyn TraceWriter>,
        task_tracking_enabled: bool,
        start_time: Instant,
    ) -> Arc<Mutex<Self>> {
        let shared = Arc::new(SharedState::new(start_time));
        let recorder = Arc::new(Mutex::new(Self {
            shared: shared.clone(),
            event_writer: EventWriter::new(writer),
        }));

        let s1 = shared.clone();
        let s2 = shared.clone();
        let s3 = shared.clone();
        let s4 = shared.clone();

        builder
            .on_thread_park(move || {
                let event = s1.make_worker_park();
                s1.record_event(event);
            })
            .on_thread_unpark(move || {
                let event = s2.make_worker_unpark();
                s2.record_event(event);
            })
            .on_before_task_poll(move |meta| {
                let task_id = TaskId::from(meta.id());
                let location = meta.spawned_at();
                let event = s3.make_poll_start(location, task_id);
                s3.record_event(event);
            })
            .on_after_task_poll(move |_meta| {
                let event = s4.make_poll_end();
                s4.record_event(event);
            });

        if task_tracking_enabled {
            let s5 = shared.clone();
            builder.on_task_spawn(move |meta| {
                let task_id = TaskId::from(meta.id());
                let location = meta.spawned_at();
                s5.record_event(RawEvent::TaskSpawn { task_id, location });
            });
        }

        #[cfg(feature = "cpu-profiling")]
        {
            let s_start = shared.clone();
            let s_stop = shared.clone();
            builder
                .on_thread_start(move || {
                    if let Ok(mut prof) = s_start.sched_profiler.lock()
                        && let Some(ref mut p) = *prof
                    {
                        let _ = p.track_current_thread();
                    }
                })
                .on_thread_stop(move || {
                    if let Ok(mut prof) = s_stop.sched_profiler.lock()
                        && let Some(ref mut p) = *prof
                    {
                        p.stop_tracking_current_thread();
                    }
                });
        }

        recorder
    }

    pub fn start_flush_task(
        recorder: Arc<Mutex<Self>>,
        interval: Duration,
    ) -> tokio::task::JoinHandle<()> {
        tokio::spawn(async move {
            let mut interval = tokio::time::interval(interval);
            loop {
                interval.tick().await;
                recorder.lock().unwrap().flush();
            }
        })
    }

    pub fn start_sampler_task(
        recorder: Arc<Mutex<Self>>,
        interval: Duration,
    ) -> tokio::task::JoinHandle<()> {
        let shared = recorder.lock().unwrap().shared.clone();
        tokio::spawn(async move {
            let mut tick = tokio::time::interval(interval);
            loop {
                tick.tick().await;
                let metrics_guard = shared.metrics.load();
                let Some(ref metrics) = **metrics_guard else {
                    continue;
                };
                shared.record_queue_sample(metrics.global_queue_depth());
            }
        })
    }
}

impl Drop for TelemetryRecorder {
    fn drop(&mut self) {
        self.flush();
    }
}

/// Cheap, cloneable handle for controlling telemetry from anywhere.
#[derive(Clone)]
pub struct TelemetryHandle {
    shared: Arc<SharedState>,
    recorder: Arc<Mutex<TelemetryRecorder>>,
}

impl TelemetryHandle {
    pub fn enable(&self) {
        self.shared.enabled.store(true, Ordering::Relaxed);
    }

    pub fn disable(&self) {
        self.shared.enabled.store(false, Ordering::Relaxed);
        self.recorder.lock().unwrap().flush();
    }

    pub fn traced_handle(&self) -> crate::traced::TracedHandle {
        crate::traced::TracedHandle {
            shared: self.shared.clone(),
        }
    }

    #[track_caller]
    pub fn spawn<F>(&self, future: F) -> tokio::task::JoinHandle<F::Output>
    where
        F: std::future::Future + Send + 'static,
        F::Output: Send + 'static,
    {
        let traced_handle = self.traced_handle();
        tokio::spawn(async move {
            let task_id = tokio::task::try_id()
                .map(TaskId::from)
                .unwrap_or(TaskId::from_u32(0));
            crate::traced::Traced::new(future, traced_handle, task_id).await
        })
    }
}

/// RAII guard returned by [`TracedRuntimeBuilder::build`].
pub struct TelemetryGuard {
    handle: TelemetryHandle,
    stop: Arc<AtomicBool>,
    thread: Option<std::thread::JoinHandle<()>>,
}

impl TelemetryGuard {
    pub fn handle(&self) -> TelemetryHandle {
        self.handle.clone()
    }

    pub fn start_time(&self) -> Instant {
        self.handle.shared.start_time
    }

    pub fn enable(&self) {
        self.handle.enable();
    }

    pub fn disable(&self) {
        self.handle.disable();
    }
}

impl Drop for TelemetryGuard {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::Release);
        if let Some(t) = self.thread.take() {
            let _ = t.join();
        }
        self.handle.recorder.lock().unwrap().flush();
    }
}

pub struct TracedRuntimeBuilder {
    task_tracking_enabled: bool,
    #[cfg(feature = "cpu-profiling")]
    cpu_profiling_config: Option<crate::telemetry::cpu_profile::CpuProfilingConfig>,
    #[cfg(feature = "cpu-profiling")]
    sched_event_config: Option<crate::telemetry::cpu_profile::SchedEventConfig>,
    #[cfg(feature = "cpu-profiling")]
    inline_callframe_symbols: bool,
}

impl TracedRuntimeBuilder {
    pub fn with_task_tracking(mut self, enabled: bool) -> Self {
        self.task_tracking_enabled = enabled;
        self
    }

    #[cfg(feature = "cpu-profiling")]
    pub fn with_cpu_profiling(
        mut self,
        config: crate::telemetry::cpu_profile::CpuProfilingConfig,
    ) -> Self {
        self.cpu_profiling_config = Some(config);
        self
    }

    #[cfg(feature = "cpu-profiling")]
    pub fn with_sched_events(
        mut self,
        config: crate::telemetry::cpu_profile::SchedEventConfig,
    ) -> Self {
        self.sched_event_config = Some(config);
        self
    }

    #[cfg(feature = "cpu-profiling")]
    pub fn with_inline_callframe_symbols(mut self, enabled: bool) -> Self {
        self.inline_callframe_symbols = enabled;
        self
    }

    pub fn build(
        self,
        mut builder: tokio::runtime::Builder,
        writer: Box<dyn TraceWriter>,
    ) -> std::io::Result<(tokio::runtime::Runtime, TelemetryGuard)> {
        let start_instant = Instant::now();
        #[cfg(feature = "cpu-profiling")]
        let start_mono_ns = crate::telemetry::cpu_profile::clock_monotonic_ns();

        #[cfg(feature = "cpu-profiling")]
        let sampler = self
            .cpu_profiling_config
            .map(|config| crate::telemetry::cpu_profile::CpuProfiler::start(config, start_mono_ns));

        #[cfg(feature = "cpu-profiling")]
        let sched = self
            .sched_event_config
            .map(|config| crate::telemetry::cpu_profile::SchedProfiler::new(config, start_mono_ns));

        let recorder = TelemetryRecorder::install(
            &mut builder,
            writer,
            self.task_tracking_enabled,
            start_instant,
        );

        #[cfg(feature = "cpu-profiling")]
        {
            let mut rec = recorder.lock().unwrap();
            let mut cpu_flush = CpuFlushState::new();
            cpu_flush.inline_callframe_symbols = self.inline_callframe_symbols;
            rec.event_writer.cpu_flush = Some(cpu_flush);
            if let Some(Ok(sampler)) = sampler {
                rec.event_writer.cpu_profiler = Some(sampler);
            }
            if let Some(Ok(sched)) = sched {
                *rec.shared.sched_profiler.lock().unwrap() = Some(sched);
            }
        }

        let runtime = builder.build()?;

        recorder
            .lock()
            .unwrap()
            .initialize(runtime.handle().clone());

        let stop = Arc::new(AtomicBool::new(false));

        let thread = {
            let rec = recorder.clone();
            let shared = recorder.lock().unwrap().shared.clone();
            let stop = stop.clone();
            std::thread::Builder::new()
                .name("telemetry-flush".into())
                .spawn(move || {
                    let flush_interval = Duration::from_millis(250);
                    let sample_interval = Duration::from_millis(10);
                    let mut last_flush = Instant::now();
                    let mut last_sample = Instant::now();

                    while !stop.load(Ordering::Acquire) {
                        std::thread::sleep(Duration::from_millis(5));

                        let now = Instant::now();
                        if now.duration_since(last_sample) >= sample_interval {
                            last_sample = now;
                            let metrics_guard = shared.metrics.load();
                            if let Some(ref metrics) = **metrics_guard {
                                shared.record_queue_sample(metrics.global_queue_depth());
                            }
                        }

                        if now.duration_since(last_flush) >= flush_interval {
                            last_flush = now;
                            rec.lock().unwrap().flush();
                        }
                    }
                })
                .expect("failed to spawn telemetry-flush thread")
        };

        let guard_shared = recorder.lock().unwrap().shared.clone();
        let guard = TelemetryGuard {
            handle: TelemetryHandle {
                shared: guard_shared,
                recorder,
            },
            stop,
            thread: Some(thread),
        };

        Ok((runtime, guard))
    }

    pub fn build_and_start(
        self,
        builder: tokio::runtime::Builder,
        writer: Box<dyn TraceWriter>,
    ) -> std::io::Result<(tokio::runtime::Runtime, TelemetryGuard)> {
        let (runtime, guard) = self.build(builder, writer)?;
        guard.enable();
        Ok((runtime, guard))
    }
}

/// Entry point for setting up a traced Tokio runtime.
pub struct TracedRuntime;

impl TracedRuntime {
    pub fn builder() -> TracedRuntimeBuilder {
        TracedRuntimeBuilder {
            task_tracking_enabled: false,
            #[cfg(feature = "cpu-profiling")]
            cpu_profiling_config: None,
            #[cfg(feature = "cpu-profiling")]
            sched_event_config: None,
            #[cfg(feature = "cpu-profiling")]
            inline_callframe_symbols: false,
        }
    }

    pub fn build(
        builder: tokio::runtime::Builder,
        writer: Box<dyn TraceWriter>,
    ) -> std::io::Result<(tokio::runtime::Runtime, TelemetryGuard)> {
        TracedRuntimeBuilder {
            task_tracking_enabled: false,
            #[cfg(feature = "cpu-profiling")]
            cpu_profiling_config: None,
            #[cfg(feature = "cpu-profiling")]
            sched_event_config: None,
            #[cfg(feature = "cpu-profiling")]
            inline_callframe_symbols: false,
        }
        .build(builder, writer)
    }

    pub fn build_and_start(
        builder: tokio::runtime::Builder,
        writer: Box<dyn TraceWriter>,
    ) -> std::io::Result<(tokio::runtime::Runtime, TelemetryGuard)> {
        TracedRuntimeBuilder {
            task_tracking_enabled: true,
            #[cfg(feature = "cpu-profiling")]
            cpu_profiling_config: None,
            #[cfg(feature = "cpu-profiling")]
            sched_event_config: None,
            #[cfg(feature = "cpu-profiling")]
            inline_callframe_symbols: false,
        }
        .build_and_start(builder, writer)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::telemetry::writer::NullWriter;
    use flush_state::FlushState;
    use smallvec::SmallVec;
    use std::panic::Location;

    #[test]
    fn test_shared_state_no_spawn_location_fields() {
        let _recorder = TelemetryRecorder::new(Box::new(NullWriter));
    }

    #[test]
    fn test_flush_state_intern() {
        let mut fs = FlushState::new();
        #[track_caller]
        fn get_loc() -> &'static Location<'static> {
            Location::caller()
        }
        let loc = get_loc();
        let id1 = fs.intern(loc);
        let id2 = fs.intern(loc);
        assert_eq!(id1, id2);
        assert_ne!(id1.as_u16(), 0);
    }

    #[test]
    fn test_flush_state_on_rotate_clears_emitted() {
        let mut fs = FlushState::new();
        #[track_caller]
        fn get_loc() -> &'static Location<'static> {
            Location::caller()
        }
        let loc = get_loc();
        let id = fs.intern(loc);
        let mut defs = SmallVec::new();
        fs.collect_def(id, &mut defs);
        assert_eq!(defs.len(), 1);
        assert!(fs.emitted_this_file.contains(&id));
        fs.on_rotate();
        assert!(!fs.emitted_this_file.contains(&id));
    }

    #[test]
    fn test_spawn_locations_resolve_after_rotation() {
        use crate::telemetry::analysis::TraceReader;
        use crate::telemetry::events::TelemetryEvent;

        let dir = tempfile::TempDir::new().unwrap();
        let base = dir.path().join("trace");

        #[track_caller]
        fn loc_a() -> &'static Location<'static> {
            Location::caller()
        }
        #[track_caller]
        fn loc_b() -> &'static Location<'static> {
            Location::caller()
        }
        let location_a = loc_a();
        let location_b = loc_b();

        let writer = crate::telemetry::writer::RotatingWriter::new(&base, 100, 100_000).unwrap();
        let mut ew = EventWriter::new(Box::new(writer));

        let locations = [
            location_a, location_b, location_a, location_b, location_a, location_b,
        ];
        for (i, loc) in locations.iter().enumerate() {
            let task_id = crate::telemetry::task_metadata::TaskId::from_u32(i as u32);
            ew.write_raw_event(RawEvent::TaskSpawn {
                task_id,
                location: loc,
            })
            .unwrap();
            ew.write_raw_event(RawEvent::PollStart {
                timestamp_nanos: (i as u64 + 1) * 1000,
                worker_id: 0,
                worker_local_queue_depth: 0,
                task_id,
                location: loc,
            })
            .unwrap();
        }
        ew.flush().unwrap();

        let mut files: Vec<_> = std::fs::read_dir(dir.path())
            .unwrap()
            .filter_map(|e| e.ok())
            .map(|e| e.path())
            .filter(|p| p.extension().map_or(false, |ext| ext == "bin"))
            .collect();
        files.sort();
        assert!(
            files.len() > 1,
            "expected multiple files from rotation, got {}",
            files.len()
        );

        let mut total_events = 0;
        for file in &files {
            let path = file.to_str().unwrap();
            let mut reader = TraceReader::new(path).unwrap();
            reader.read_header().unwrap();
            let events = reader.read_all().unwrap();

            for ev in &events {
                if let TelemetryEvent::PollStart { spawn_loc_id, .. } = ev {
                    let loc = reader.spawn_locations.get(spawn_loc_id).unwrap_or_else(|| {
                        panic!(
                            "file {path:?}: spawn_loc_id {spawn_loc_id:?} has no definition {:#?}",
                            reader.spawn_locations
                        )
                    });
                    assert!(
                        loc.contains(':'),
                        "location should be file:line:col, got {loc:?}"
                    );
                }
            }

            for (task_id, spawn_loc_id) in &reader.task_spawn_locs {
                reader.spawn_locations.get(spawn_loc_id).unwrap_or_else(|| {
                    panic!("file {path:?}: task {task_id:?} spawn_loc_id {spawn_loc_id:?} has no definition")
                });
            }

            total_events += events.len();
        }
        assert_eq!(
            total_events, 6,
            "all PollStart events should be readable across files"
        );
    }

    #[cfg(feature = "cpu-profiling")]
    mod rotation_proptest {
        use super::*;
        use crate::telemetry::analysis::TraceReader;
        use crate::telemetry::events::{CpuSampleSource, TelemetryEvent, UNKNOWN_WORKER};
        use crate::telemetry::task_metadata::{SpawnLocationId, TaskId};
        use crate::telemetry::writer::RotatingWriter;
        use cpu_flush_state::CpuFlushState;
        use proptest::prelude::*;
        use std::collections::HashSet;

        #[derive(Debug, Clone)]
        enum FlushOp {
            CpuSample {
                worker_id: usize,
                tid: u32,
                callchain: Vec<u64>,
            },
            PollStart {
                location_idx: usize,
            },
        }

        fn arb_flush_op() -> impl Strategy<Value = FlushOp> {
            prop_oneof![
                (
                    prop::bool::ANY,
                    0u32..4,
                    prop::collection::vec(0u64..8, 0..3),
                )
                    .prop_map(|(is_worker, tid, callchain)| {
                        FlushOp::CpuSample {
                            worker_id: if is_worker { 0 } else { UNKNOWN_WORKER },
                            tid,
                            callchain,
                        }
                    }),
                (0usize..3).prop_map(|idx| FlushOp::PollStart { location_idx: idx }),
            ]
        }

        #[derive(Debug, Clone)]
        struct FlushRound {
            cpu_ops: Vec<FlushOp>,
            raw_ops: Vec<FlushOp>,
        }

        fn arb_flush_round() -> impl Strategy<Value = FlushRound> {
            (
                prop::collection::vec(arb_flush_op(), 0..12).prop_map(|ops| {
                    ops.into_iter()
                        .filter(|o| matches!(o, FlushOp::CpuSample { .. }))
                        .collect()
                }),
                prop::collection::vec(arb_flush_op(), 0..12).prop_map(|ops| {
                    ops.into_iter()
                        .filter(|o| matches!(o, FlushOp::PollStart { .. }))
                        .collect()
                }),
            )
                .prop_map(|(cpu_ops, raw_ops)| FlushRound { cpu_ops, raw_ops })
        }

        fn execute_flush_round(
            round: &FlushRound,
            ew: &mut EventWriter,
            locations: &[&'static Location<'static>],
            timestamp: &mut u64,
            expected_raw: &mut usize,
        ) {
            for op in &round.cpu_ops {
                if let FlushOp::CpuSample {
                    worker_id,
                    tid,
                    callchain,
                } = op
                {
                    let event = TelemetryEvent::CpuSample {
                        timestamp_nanos: *timestamp,
                        worker_id: *worker_id,
                        tid: *tid,
                        source: CpuSampleSource::CpuProfile,
                        callchain: callchain.clone(),
                    };
                    *timestamp += 1;
                    ew.write_cpu_event(&event);
                }
            }

            for op in &round.raw_ops {
                if let FlushOp::PollStart { location_idx } = op {
                    let loc = locations[*location_idx];
                    let task_id = TaskId::from_u32(*timestamp as u32);
                    let raw = RawEvent::PollStart {
                        timestamp_nanos: *timestamp,
                        worker_id: 0,
                        worker_local_queue_depth: 0,
                        task_id,
                        location: loc,
                    };
                    *timestamp += 1;

                    match ew.write_raw_event(raw).unwrap() {
                        WriteAtomicResult::Written => {
                            *expected_raw += 1;
                        }
                        WriteAtomicResult::OversizedBatch => {}
                        WriteAtomicResult::Rotated => {
                            panic!("double rotation on raw event retry");
                        }
                    }
                }
            }
        }

        fn verify_files(dir: &std::path::Path) -> usize {
            let mut files: Vec<_> = std::fs::read_dir(dir)
                .unwrap()
                .filter_map(|e| e.ok())
                .map(|e| e.path())
                .filter(|p| p.extension().map_or(false, |ext| ext == "bin"))
                .collect();
            files.sort();

            let mut total_raw = 0;

            for file in &files {
                let path_str = file.to_str().unwrap();
                let mut reader = TraceReader::new(path_str)
                    .unwrap_or_else(|e| panic!("failed to open {path_str}: {e}"));
                reader.read_header().unwrap();

                let mut events = Vec::new();
                while let Some(ev) = reader.read_raw_event().unwrap() {
                    events.push(ev);
                }

                let mut spawn_loc_defs: HashSet<SpawnLocationId> = HashSet::new();
                let mut thread_name_defs: HashSet<u32> = HashSet::new();
                let mut callframe_defs: HashSet<u64> = HashSet::new();

                for ev in &events {
                    match ev {
                        TelemetryEvent::SpawnLocationDef { id, location } => {
                            assert!(
                                location.contains(':'),
                                "{path_str}: SpawnLocationDef has bad location: {location:?}"
                            );
                            spawn_loc_defs.insert(*id);
                        }
                        TelemetryEvent::ThreadNameDef { tid, .. } => {
                            thread_name_defs.insert(*tid);
                        }
                        TelemetryEvent::CallframeDef { address, .. } => {
                            callframe_defs.insert(*address);
                        }
                        TelemetryEvent::PollStart { spawn_loc_id, .. } => {
                            assert!(
                                spawn_loc_defs.contains(spawn_loc_id),
                                "{path_str}: PollStart references spawn_loc_id {spawn_loc_id:?} but no SpawnLocationDef in this file. Defs: {spawn_loc_defs:?}"
                            );
                            total_raw += 1;
                        }
                        TelemetryEvent::CpuSample {
                            worker_id,
                            tid,
                            callchain,
                            ..
                        } => {
                            if *worker_id == UNKNOWN_WORKER {
                                assert!(
                                    thread_name_defs.contains(tid),
                                    "{path_str}: CpuSample for non-worker tid {tid} has no ThreadNameDef. Defs: {thread_name_defs:?}"
                                );
                            }
                            for addr in callchain {
                                assert!(
                                    callframe_defs.contains(addr),
                                    "{path_str}: CpuSample references callchain address {addr:#x} but no CallframeDef. Defs: {callframe_defs:?}"
                                );
                            }
                        }
                        _ => {}
                    }
                }
            }
            total_raw
        }

        proptest! {
            #![proptest_config(ProptestConfig::with_cases(256))]

            #[test]
            fn rotation_preserves_self_containedness(
                rounds in prop::collection::vec(arb_flush_round(), 1..6),
                max_file_size in 60u64..300,
            ) {
                let dir = tempfile::TempDir::new().unwrap();
                let base = dir.path().join("trace");

                let writer = RotatingWriter::new(&base, max_file_size, 1_000_000).unwrap();

                let mut ew = EventWriter::new(Box::new(writer));
                let mut cpu = CpuFlushState::new();
                cpu.inline_callframe_symbols = true;
                for tid in 0u32..4 {
                    cpu.thread_name_intern.insert(tid, format!("thread-{tid}"));
                }
                ew.cpu_flush = Some(cpu);

                #[track_caller]
                fn loc0() -> &'static Location<'static> { Location::caller() }
                #[track_caller]
                fn loc1() -> &'static Location<'static> { Location::caller() }
                #[track_caller]
                fn loc2() -> &'static Location<'static> { Location::caller() }
                let locations: Vec<&'static Location<'static>> = vec![loc0(), loc1(), loc2()];

                let mut timestamp = 1u64;
                let mut expected_raw = 0usize;

                for round in &rounds {
                    execute_flush_round(
                        round,
                        &mut ew,
                        &locations,
                        &mut timestamp,
                        &mut expected_raw,
                    );
                }
                ew.flush().unwrap();

                let actual_raw = verify_files(dir.path());

                prop_assert_eq!(
                    actual_raw, expected_raw,
                    "raw event count mismatch: expected {}, got {}", expected_raw, actual_raw
                );
            }
        }
    }
}
