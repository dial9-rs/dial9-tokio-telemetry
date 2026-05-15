use crate::telemetry::events::{CpuSampleSource, TelemetryEvent};
use crate::telemetry::format::{self, TelemetryEventRef, WorkerId};
use crate::telemetry::task_metadata::TaskId;
use dial9_trace_format::FieldValue;
use dial9_trace_format::InternedString;
use dial9_trace_format::decoder::{Decoder, StringPool};
use std::cmp::Reverse;
use std::collections::HashMap;
use std::io::{Read as _, Result};

/// Reads a trace file written in the `dial9-trace-format` binary format.
///
/// Decodes the entire file eagerly and populates lookup tables for spawn
/// locations (from the string pool), task→spawn-location mappings, callframe
/// symbols, thread names, and segment metadata.
#[derive(Debug)]
pub struct TraceReader {
    /// All decoded events (including metadata like TaskSpawn).
    pub all_events: Vec<TelemetryEvent>,
    /// Runtime events only (excludes TaskSpawn, ThreadNameDef, SegmentMetadata).
    pub runtime_events: Vec<TelemetryEvent>,
    /// Spawn location strings keyed by `InternedString` from the string pool.
    pub spawn_locations: HashMap<InternedString, String>,
    /// Task ID → spawn location mapping built from TaskSpawn events.
    pub task_spawn_locs: HashMap<TaskId, InternedString>,
    /// OS tid → thread name mapping built from ThreadNameDef events.
    pub thread_names: HashMap<u32, String>,
    /// Key-value metadata from the most recent SegmentMetadata event.
    pub segment_metadata: Vec<(String, String)>,
}

impl TraceReader {
    /// Read and decode a trace file at the given path.
    pub fn new(path: &str) -> Result<Self> {
        let data = read_trace_file(path)?;
        let mut dec = Decoder::new(&data).ok_or_else(|| {
            std::io::Error::new(std::io::ErrorKind::InvalidData, "invalid trace header")
        })?;

        let mut spawn_locations = HashMap::new();
        let mut task_spawn_locs = HashMap::new();
        let mut thread_names = HashMap::new();
        let mut segment_metadata = Vec::new();
        let mut events = Vec::new();

        // Resolve InternedString fields (spawn_loc, thread_name) inside the
        // callback where the string pool is still valid for the current batch.
        // After a mid-stream header the pool resets, so deferred resolution
        // would use the wrong pool for earlier batches.
        dec.for_each_event(|ev| {
            if let Some(r) = format::decode_ref(ev.name, ev.timestamp_ns, ev.fields, ev.schema) {
                match &r {
                    TelemetryEventRef::PollStart(e) => {
                        populate_spawn_loc(&mut spawn_locations, e.spawn_loc, ev.string_pool);
                    }
                    TelemetryEventRef::TaskSpawn(e) => {
                        populate_spawn_loc(&mut spawn_locations, e.spawn_loc, ev.string_pool);
                        task_spawn_locs.insert(e.task_id, e.spawn_loc);
                    }
                    TelemetryEventRef::SegmentMetadata(e) => {
                        segment_metadata = e
                            .entries
                            .iter()
                            .map(|(k, v)| (k.to_string(), v.to_string()))
                            .collect();
                    }
                    _ => {}
                }
                let owned = format::to_owned_event(r, ev.string_pool, ev.stack_pool);
                if let TelemetryEvent::CpuSample {
                    tid,
                    thread_name: Some(ref name),
                    ..
                } = owned
                {
                    thread_names.insert(tid, name.clone());
                }
                events.push(owned);
            } else {
                // Unknown event name: capture as Custom, resolving any
                // pooled fields to owned data while the segment-local pools
                // are still available.
                let fields = ev
                    .field_names()
                    .zip(ev.fields.iter())
                    .map(|(name, val)| {
                        let owned = match val {
                            dial9_trace_format::types::FieldValueRef::PooledString(id) => {
                                let s = ev
                                    .string_pool
                                    .get(*id)
                                    .unwrap_or("<unresolved>")
                                    .to_string();
                                FieldValue::String(s)
                            }
                            dial9_trace_format::types::FieldValueRef::PooledStackFrames(id) => {
                                let frames = ev
                                    .stack_pool
                                    .get(*id)
                                    .expect("stack pool entry must exist for PooledStackFrames")
                                    .to_vec();
                                FieldValue::StackFrames(frames.into())
                            }
                            other => other.to_owned(),
                        };
                        (name.to_string(), owned)
                    })
                    .collect();
                events.push(TelemetryEvent::Custom {
                    timestamp_nanos: ev.timestamp_ns,
                    name: ev.name.to_string(),
                    fields,
                });
            }
        })
        .map_err(|e| std::io::Error::new(std::io::ErrorKind::InvalidData, e.to_string()))?;

        let all_events: Vec<TelemetryEvent> = events;

        let runtime_events = all_events
            .iter()
            .filter(|e| {
                !matches!(
                    e,
                    TelemetryEvent::TaskSpawn { .. }
                        | TelemetryEvent::ThreadNameDef { .. }
                        | TelemetryEvent::SegmentMetadata { .. }
                        | TelemetryEvent::ClockSync { .. }
                )
            })
            .cloned()
            .collect();

        Ok(Self {
            all_events,
            runtime_events,
            spawn_locations,
            task_spawn_locs,
            thread_names,
            segment_metadata,
        })
    }
}

fn read_trace_file(path: &str) -> Result<Vec<u8>> {
    let data = std::fs::read(path)?;
    maybe_decompress_gzip(data)
}

fn maybe_decompress_gzip(data: Vec<u8>) -> Result<Vec<u8>> {
    const GZIP_MAGIC: [u8; 2] = [0x1f, 0x8b];

    if !data.starts_with(&GZIP_MAGIC) {
        return Ok(data);
    }

    let mut decoder = flate2::read::GzDecoder::new(&data[..]);
    let mut decompressed = Vec::new();
    decoder.read_to_end(&mut decompressed).map_err(|error| {
        std::io::Error::new(
            std::io::ErrorKind::InvalidData,
            format!("failed to decompress gzip trace: {error}"),
        )
    })?;
    Ok(decompressed)
}

fn populate_spawn_loc(
    map: &mut HashMap<InternedString, String>,
    interned: dial9_trace_format::InternedString,
    pool: &StringPool,
) {
    if !map.contains_key(&interned)
        && let Some(s) = pool.get(interned)
    {
        map.insert(interned, s.to_string());
    }
}

/// Aggregated statistics for a single Tokio worker thread.
#[derive(Debug, Default)]
pub struct WorkerStats {
    /// Number of task polls executed by this worker.
    pub poll_count: usize,
    /// Number of times this worker parked (went idle).
    pub park_count: usize,
    /// Number of times this worker was unparked (resumed).
    pub unpark_count: usize,
    /// Cumulative time spent polling tasks, in nanoseconds.
    pub total_poll_time_ns: u64,
    /// Maximum observed local queue depth for this worker.
    pub max_local_queue: usize,
    /// Maximum OS scheduling wait observed during any single unpark, in nanoseconds.
    pub max_sched_wait_ns: u64,
    /// Cumulative OS scheduling wait across all unparks, in nanoseconds.
    pub total_sched_wait_ns: u64,
}

/// Aggregated poll statistics for a single spawn location.
#[derive(Debug, Default)]
pub struct SpawnLocationStats {
    /// Number of polls from tasks spawned at this location.
    pub poll_count: usize,
    /// Cumulative poll time for tasks from this location, in nanoseconds.
    pub total_poll_time_ns: u64,
}

/// Summary of a decoded trace: event counts, timing, and per-worker statistics.
#[derive(Debug)]
pub struct TraceAnalysis {
    /// Total number of events in the trace.
    pub total_events: usize,
    /// Wall-clock duration of the trace in nanoseconds.
    pub duration_ns: u64,
    /// Per-worker aggregated statistics.
    pub worker_stats: HashMap<WorkerId, WorkerStats>,
    /// Maximum observed global queue depth across all samples.
    pub max_global_queue: usize,
    /// Average global queue depth across all samples.
    pub avg_global_queue: f64,
    /// Per-spawn-location statistics (only populated when task tracking is enabled).
    pub spawn_location_stats: HashMap<InternedString, SpawnLocationStats>,
}

/// Build a sorted list of (timestamp, global_queue_depth) from QueueSample events.
fn build_global_queue_timeline(events: &[TelemetryEvent]) -> Vec<(u64, usize)> {
    let mut timeline: Vec<(u64, usize)> = events
        .iter()
        .filter_map(|e| match e {
            TelemetryEvent::QueueSample {
                timestamp_nanos,
                global_queue_depth,
            } => Some((*timestamp_nanos, *global_queue_depth)),
            _ => None,
        })
        .collect();
    timeline.sort_by_key(|&(ts, _)| ts);
    timeline
}

/// Look up the most recent global queue depth at or before the given timestamp.
/// Returns 0 if no sample has been recorded yet.
fn lookup_global_queue_depth(timeline: &[(u64, usize)], timestamp: u64) -> usize {
    match timeline.binary_search_by_key(&timestamp, |&(ts, _)| ts) {
        Ok(idx) => timeline[idx].1,
        Err(0) => 0,
        Err(idx) => timeline[idx - 1].1,
    }
}

/// Analyze a slice of telemetry events and produce a [`TraceAnalysis`] summary.
pub fn analyze_trace(events: &[TelemetryEvent]) -> TraceAnalysis {
    let mut worker_stats: HashMap<WorkerId, WorkerStats> = HashMap::new();
    let mut poll_starts: HashMap<WorkerId, u64> = HashMap::new();
    // Track spawn_loc at PollStart for computing per-location poll time at PollEnd
    let mut poll_start_locs: HashMap<WorkerId, InternedString> = HashMap::new();
    let mut spawn_location_stats: HashMap<InternedString, SpawnLocationStats> = HashMap::new();
    let mut max_global_queue = 0;
    let mut global_queue_sum = 0u64;
    let mut global_queue_count = 0u64;

    let start_time = events
        .first()
        .and_then(|e| e.timestamp_nanos())
        .unwrap_or(0);
    let end_time = events.last().and_then(|e| e.timestamp_nanos()).unwrap_or(0);

    for event in events {
        match event {
            TelemetryEvent::QueueSample {
                global_queue_depth, ..
            } => {
                max_global_queue = max_global_queue.max(*global_queue_depth);
                global_queue_sum += *global_queue_depth as u64;
                global_queue_count += 1;
            }
            TelemetryEvent::PollStart {
                timestamp_nanos,
                worker_id,
                worker_local_queue_depth,
                spawn_loc,
                ..
            } => {
                let stats = worker_stats.entry(*worker_id).or_default();
                stats.max_local_queue = stats.max_local_queue.max(*worker_local_queue_depth);
                stats.poll_count += 1;
                poll_starts.insert(*worker_id, *timestamp_nanos);
                if spawn_loc.raw_id() != 0 {
                    spawn_location_stats
                        .entry(*spawn_loc)
                        .or_default()
                        .poll_count += 1;
                    poll_start_locs.insert(*worker_id, *spawn_loc);
                }
            }
            TelemetryEvent::PollEnd {
                timestamp_nanos,
                worker_id,
            } => {
                let stats = worker_stats.entry(*worker_id).or_default();
                if let Some(start) = poll_starts.get(worker_id) {
                    let duration = timestamp_nanos.saturating_sub(*start);
                    stats.total_poll_time_ns += duration;
                    if let Some(loc_id) = poll_start_locs.remove(worker_id) {
                        spawn_location_stats
                            .entry(loc_id)
                            .or_default()
                            .total_poll_time_ns += duration;
                    }
                }
            }
            TelemetryEvent::WorkerPark {
                worker_id,
                worker_local_queue_depth,
                ..
            } => {
                let stats = worker_stats.entry(*worker_id).or_default();
                stats.max_local_queue = stats.max_local_queue.max(*worker_local_queue_depth);
                stats.park_count += 1;
            }
            TelemetryEvent::WorkerUnpark {
                worker_id,
                worker_local_queue_depth,
                sched_wait_delta_nanos,
                ..
            } => {
                let stats = worker_stats.entry(*worker_id).or_default();
                stats.max_local_queue = stats.max_local_queue.max(*worker_local_queue_depth);
                stats.unpark_count += 1;
                stats.total_sched_wait_ns += sched_wait_delta_nanos;
                stats.max_sched_wait_ns = stats.max_sched_wait_ns.max(*sched_wait_delta_nanos);
            }
            TelemetryEvent::TaskSpawn { .. }
            | TelemetryEvent::TaskTerminate { .. }
            | TelemetryEvent::CpuSample { .. }
            | TelemetryEvent::TaskDump { .. }
            | TelemetryEvent::ThreadNameDef { .. }
            | TelemetryEvent::WakeEvent { .. }
            | TelemetryEvent::SegmentMetadata { .. }
            | TelemetryEvent::ClockSync { .. }
            | TelemetryEvent::Custom { .. } => {}
        }
    }

    TraceAnalysis {
        total_events: events.len(),
        duration_ns: end_time.saturating_sub(start_time),
        worker_stats,
        max_global_queue,
        avg_global_queue: if global_queue_count > 0 {
            global_queue_sum as f64 / global_queue_count as f64
        } else {
            0.0
        },
        spawn_location_stats,
    }
}

/// Compute wake→poll scheduling delays from wake events and poll starts.
/// Returns a sorted vec of delay durations in nanoseconds.
pub fn compute_wake_to_poll_delays(events: &[TelemetryEvent]) -> Vec<u64> {
    // Index: task_id → sorted vec of wake timestamps
    let mut wakes_by_task: HashMap<TaskId, Vec<u64>> = HashMap::new();
    for e in events {
        if let TelemetryEvent::WakeEvent {
            timestamp_nanos,
            woken_task_id,
            ..
        } = e
        {
            wakes_by_task
                .entry(*woken_task_id)
                .or_default()
                .push(*timestamp_nanos);
        }
    }
    for v in wakes_by_task.values_mut() {
        v.sort_unstable();
    }

    let mut delays = Vec::new();
    for e in events {
        if let TelemetryEvent::PollStart {
            timestamp_nanos,
            task_id,
            ..
        } = e
            && let Some(wakes) = wakes_by_task.get(task_id)
        {
            // Binary search for last wake <= poll start
            let idx = wakes.partition_point(|&t| t <= *timestamp_nanos);
            if idx > 0 {
                let delay = timestamp_nanos - wakes[idx - 1];
                if delay > 0 && delay < 1_000_000_000 {
                    delays.push(delay);
                }
            }
        }
    }
    delays.sort_unstable();
    delays
}

/// An active period between WorkerUnpark and WorkerPark, with scheduling ratio.
#[derive(Debug)]
pub struct ActivePeriod {
    /// Worker thread that was active during this period.
    pub worker_id: WorkerId,
    /// Timestamp when the worker was unparked (nanos).
    pub start_ns: u64,
    /// Timestamp when the worker parked again (nanos).
    pub end_ns: u64,
    /// CPU time consumed during this active period (nanos).
    pub cpu_delta_ns: u64,
    /// Fraction of wall time the thread was actually on-CPU (0.0–1.0).
    pub scheduling_ratio: f64,
}

/// Compute scheduling ratios for each active period (unpark→park) per worker.
pub fn compute_active_periods(events: &[TelemetryEvent]) -> Vec<ActivePeriod> {
    let mut periods = Vec::new();
    // Track (wall_ns, cpu_ns) at unpark
    let mut unpark_state: HashMap<WorkerId, (u64, u64)> = HashMap::new();

    for event in events {
        match event {
            TelemetryEvent::WorkerUnpark {
                timestamp_nanos,
                worker_id,
                cpu_time_nanos,
                ..
            } => {
                unpark_state.insert(*worker_id, (*timestamp_nanos, *cpu_time_nanos));
            }
            TelemetryEvent::WorkerPark {
                timestamp_nanos,
                worker_id,
                cpu_time_nanos,
                ..
            } => {
                if let Some((start_wall, start_cpu)) = unpark_state.remove(worker_id) {
                    let wall_delta = timestamp_nanos.saturating_sub(start_wall);
                    let cpu_delta = cpu_time_nanos.saturating_sub(start_cpu);
                    let ratio = if wall_delta > 0 {
                        (cpu_delta as f64 / wall_delta as f64).min(1.0)
                    } else {
                        1.0
                    };
                    periods.push(ActivePeriod {
                        worker_id: *worker_id,
                        start_ns: start_wall,
                        end_ns: *timestamp_nanos,
                        cpu_delta_ns: cpu_delta,
                        scheduling_ratio: ratio,
                    });
                }
            }
            _ => {}
        }
    }
    periods
}

/// Print a human-readable summary of a [`TraceAnalysis`] to stdout.
pub fn print_analysis(analysis: &TraceAnalysis, spawn_locations: &HashMap<InternedString, String>) {
    println!("\n=== Trace Analysis ===");
    println!("Total events: {}", analysis.total_events);
    println!(
        "Duration: {:.2}s",
        analysis.duration_ns as f64 / 1_000_000_000.0
    );
    println!("Max global queue depth: {}", analysis.max_global_queue);
    println!("Avg global queue depth: {:.2}", analysis.avg_global_queue);

    println!("\n=== Worker Statistics ===");
    for (worker_id, stats) in &analysis.worker_stats {
        println!("\nWorker {}:", worker_id);
        println!("  Polls: {}", stats.poll_count);
        println!("  Parks: {}", stats.park_count);
        println!("  Unparks: {}", stats.unpark_count);
        println!(
            "  Avg poll time: {:.2}µs",
            if stats.poll_count > 0 {
                stats.total_poll_time_ns as f64 / stats.poll_count as f64 / 1000.0
            } else {
                0.0
            }
        );
        println!("  Max local queue: {}", stats.max_local_queue);
        if stats.unpark_count > 0 {
            println!(
                "  Sched wait: avg {:.1}µs, max {:.1}µs",
                stats.total_sched_wait_ns as f64 / stats.unpark_count as f64 / 1000.0,
                stats.max_sched_wait_ns as f64 / 1000.0,
            );
        }
    }

    if !analysis.spawn_location_stats.is_empty() {
        println!("\n=== Spawn Locations (by poll count) ===");
        let mut locs: Vec<_> = analysis.spawn_location_stats.iter().collect();
        locs.sort_by_key(|b| Reverse(b.1.poll_count));
        for (id, stats) in locs {
            let name = spawn_locations
                .get(id)
                .map(|s| s.as_str())
                .unwrap_or("<unknown>");
            let avg_poll_us = if stats.poll_count > 0 {
                stats.total_poll_time_ns as f64 / stats.poll_count as f64 / 1000.0
            } else {
                0.0
            };
            println!(
                "  {} — {} polls, avg {:.2}µs",
                name, stats.poll_count, avg_poll_us
            );
        }
    }
}

/// Detect periods where a worker was parked while the global queue had pending work.
///
/// Uses the global queue sample timeline to look up the queue depth at unpark time,
/// since global_queue_depth is only recorded on QueueSample events.
pub fn detect_idle_workers(events: &[TelemetryEvent]) -> Vec<(WorkerId, u64, usize)> {
    let global_queue_timeline = build_global_queue_timeline(events);
    let mut idle_periods = Vec::new();
    let mut worker_park_times: HashMap<WorkerId, u64> = HashMap::new();

    for event in events {
        match event {
            TelemetryEvent::WorkerPark {
                timestamp_nanos,
                worker_id,
                ..
            } => {
                worker_park_times.insert(*worker_id, *timestamp_nanos);
            }
            TelemetryEvent::WorkerUnpark {
                timestamp_nanos,
                worker_id,
                ..
            } => {
                if let Some(park_time) = worker_park_times.remove(worker_id) {
                    let idle_duration = timestamp_nanos.saturating_sub(park_time);
                    let global_queue_at_unpark =
                        lookup_global_queue_depth(&global_queue_timeline, *timestamp_nanos);
                    if idle_duration > 1_000_000 && global_queue_at_unpark > 0 {
                        idle_periods.push((*worker_id, idle_duration, global_queue_at_unpark));
                    }
                }
            }
            _ => {}
        }
    }

    idle_periods
}

/// A poll that exceeded the given duration threshold.
#[derive(Debug)]
pub struct LongPoll {
    /// Worker that executed the poll.
    pub worker_id: WorkerId,
    /// Poll start timestamp (nanos).
    pub start_ns: u64,
    /// Poll end timestamp (nanos).
    pub end_ns: u64,
    /// Duration of the poll in nanoseconds.
    pub duration_ns: u64,
    /// Task that was being polled.
    pub task_id: TaskId,
    /// Spawn location of the task.
    pub spawn_loc: InternedString,
}

/// Detect polls that exceed `threshold_ns` nanoseconds.
///
/// Returns long polls in timestamp order. Each entry captures the worker,
/// time range, and task metadata (when task tracking is enabled).
pub fn detect_long_polls(events: &[TelemetryEvent], threshold_ns: u64) -> Vec<LongPoll> {
    let mut long_polls = Vec::new();
    let mut poll_starts: HashMap<WorkerId, (u64, TaskId, InternedString)> = HashMap::new();

    for event in events {
        match event {
            TelemetryEvent::PollStart {
                timestamp_nanos,
                worker_id,
                task_id,
                spawn_loc,
                ..
            } => {
                poll_starts.insert(*worker_id, (*timestamp_nanos, *task_id, *spawn_loc));
            }
            TelemetryEvent::PollEnd {
                timestamp_nanos,
                worker_id,
            } => {
                if let Some((start, task_id, spawn_loc)) = poll_starts.remove(worker_id) {
                    let duration = timestamp_nanos.saturating_sub(start);
                    if duration >= threshold_ns {
                        long_polls.push(LongPoll {
                            worker_id: *worker_id,
                            start_ns: start,
                            end_ns: *timestamp_nanos,
                            duration_ns: duration,
                            task_id,
                            spawn_loc,
                        });
                    }
                }
            }
            _ => {}
        }
    }
    long_polls
}

/// A park period where the OS scheduler delayed the worker thread.
#[derive(Debug)]
pub struct SchedDelay {
    /// Worker that experienced the scheduling delay.
    pub worker_id: WorkerId,
    /// Timestamp when the worker parked (nanos).
    pub park_ns: u64,
    /// Timestamp when the worker was unparked (nanos).
    pub unpark_ns: u64,
    /// OS scheduling wait during this park period (nanos).
    pub sched_wait_ns: u64,
}

/// Detect park periods where OS scheduling wait exceeded `threshold_ns`.
///
/// `sched_wait_delta_nanos` on `WorkerUnpark` reports how long the thread was
/// runnable but not scheduled by the OS during the preceding park.
pub fn detect_sched_delays(events: &[TelemetryEvent], threshold_ns: u64) -> Vec<SchedDelay> {
    let mut delays = Vec::new();
    let mut park_times: HashMap<WorkerId, u64> = HashMap::new();

    for event in events {
        match event {
            TelemetryEvent::WorkerPark {
                timestamp_nanos,
                worker_id,
                ..
            } => {
                park_times.insert(*worker_id, *timestamp_nanos);
            }
            TelemetryEvent::WorkerUnpark {
                timestamp_nanos,
                worker_id,
                sched_wait_delta_nanos,
                ..
            } => {
                if let Some(park_ns) = park_times.remove(worker_id)
                    && *sched_wait_delta_nanos >= threshold_ns
                {
                    delays.push(SchedDelay {
                        worker_id: *worker_id,
                        park_ns,
                        unpark_ns: *timestamp_nanos,
                        sched_wait_ns: *sched_wait_delta_nanos,
                    });
                }
            }
            _ => {}
        }
    }
    delays
}

/// A wake-to-poll scheduling delay that exceeded a threshold.
#[derive(Debug)]
pub struct WakeDelay {
    /// Worker that polled the task after the wake.
    pub worker_id: WorkerId,
    /// Timestamp of the wake event (nanos).
    pub wake_ns: u64,
    /// Timestamp of the subsequent poll start (nanos).
    pub poll_ns: u64,
    /// Delay between wake and poll in nanoseconds.
    pub delay_ns: u64,
    /// Task that experienced the scheduling delay.
    pub task_id: TaskId,
}

/// Detect wake-to-poll delays exceeding `threshold_ns`.
///
/// For each `PollStart`, finds the most recent wake for that task and reports
/// the delay if it exceeds the threshold. Delays >= 1s are discarded as likely
/// representing idle tasks rather than scheduling problems.
pub fn detect_wake_delays(events: &[TelemetryEvent], threshold_ns: u64) -> Vec<WakeDelay> {
    const MAX_REASONABLE_DELAY_NS: u64 = 1_000_000_000;

    let mut wakes_by_task: HashMap<TaskId, Vec<u64>> = HashMap::new();
    for event in events {
        if let TelemetryEvent::WakeEvent {
            timestamp_nanos,
            woken_task_id,
            ..
        } = event
        {
            wakes_by_task
                .entry(*woken_task_id)
                .or_default()
                .push(*timestamp_nanos);
        }
    }
    for v in wakes_by_task.values_mut() {
        v.sort_unstable();
    }

    let mut delays = Vec::new();
    for event in events {
        if let TelemetryEvent::PollStart {
            timestamp_nanos,
            worker_id,
            task_id,
            ..
        } = event
            && let Some(wakes) = wakes_by_task.get(task_id)
        {
            let idx = wakes.partition_point(|&t| t <= *timestamp_nanos);
            if idx > 0 {
                let delay = timestamp_nanos.saturating_sub(wakes[idx - 1]);
                if delay >= threshold_ns && delay < MAX_REASONABLE_DELAY_NS {
                    delays.push(WakeDelay {
                        worker_id: *worker_id,
                        wake_ns: wakes[idx - 1],
                        poll_ns: *timestamp_nanos,
                        delay_ns: delay,
                        task_id: *task_id,
                    });
                }
            }
        }
    }
    delays
}

/// A poll that had CPU or scheduler samples collected during its execution.
#[derive(Debug)]
pub struct SampledPoll {
    /// Worker that executed the poll.
    pub worker_id: WorkerId,
    /// Poll start timestamp (nanos).
    pub start_ns: u64,
    /// Poll end timestamp (nanos).
    pub end_ns: u64,
    /// Task that was being polled.
    pub task_id: TaskId,
    /// Spawn location of the task.
    pub spawn_loc: InternedString,
    /// Number of CPU profile samples collected during this poll.
    pub cpu_sample_count: usize,
    /// Number of scheduler event samples collected during this poll.
    pub sched_sample_count: usize,
}

/// Find polls that had CPU profile or scheduler event samples collected during
/// their execution. Correlates `CpuSample` events with poll time ranges on the
/// same worker.
pub fn detect_sampled_polls(events: &[TelemetryEvent]) -> Vec<SampledPoll> {
    struct PollSpan {
        worker_id: WorkerId,
        start_ns: u64,
        end_ns: u64,
        task_id: TaskId,
        spawn_loc: InternedString,
        cpu_samples: usize,
        sched_samples: usize,
    }

    // First pass: build poll spans per worker
    let mut polls: Vec<PollSpan> = Vec::new();
    let mut poll_starts: HashMap<WorkerId, (u64, TaskId, InternedString)> = HashMap::new();

    for event in events {
        match event {
            TelemetryEvent::PollStart {
                timestamp_nanos,
                worker_id,
                task_id,
                spawn_loc,
                ..
            } => {
                poll_starts.insert(*worker_id, (*timestamp_nanos, *task_id, *spawn_loc));
            }
            TelemetryEvent::PollEnd {
                timestamp_nanos,
                worker_id,
            } => {
                if let Some((start, task_id, spawn_loc)) = poll_starts.remove(worker_id) {
                    polls.push(PollSpan {
                        worker_id: *worker_id,
                        start_ns: start,
                        end_ns: *timestamp_nanos,
                        task_id,
                        spawn_loc,
                        cpu_samples: 0,
                        sched_samples: 0,
                    });
                }
            }
            _ => {}
        }
    }

    // Sort polls by (worker_id, start_ns) for binary search
    polls.sort_unstable_by_key(|p| (p.worker_id, p.start_ns));

    // Second pass: attribute each CpuSample to a poll
    for event in events {
        if let TelemetryEvent::CpuSample {
            timestamp_nanos,
            worker_id,
            source,
            ..
        } = event
        {
            let start_idx = polls.partition_point(|p| p.worker_id < *worker_id);
            let end_idx = polls.partition_point(|p| p.worker_id <= *worker_id);
            let worker_polls = &mut polls[start_idx..end_idx];

            let idx = worker_polls.partition_point(|p| p.start_ns <= *timestamp_nanos);
            if idx > 0 && *timestamp_nanos <= worker_polls[idx - 1].end_ns {
                match source {
                    CpuSampleSource::CpuProfile => worker_polls[idx - 1].cpu_samples += 1,
                    CpuSampleSource::SchedEvent => worker_polls[idx - 1].sched_samples += 1,
                }
            }
        }
    }

    // Collect polls that had any samples, grouped by worker
    polls
        .into_iter()
        .filter(|p| p.cpu_samples > 0 || p.sched_samples > 0)
        .map(|p| SampledPoll {
            worker_id: p.worker_id,
            start_ns: p.start_ns,
            end_ns: p.end_ns,
            task_id: p.task_id,
            spawn_loc: p.spawn_loc,
            cpu_sample_count: p.cpu_samples,
            sched_sample_count: p.sched_samples,
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::telemetry::format::WorkerId;
    use crate::telemetry::task_metadata::UNKNOWN_TASK_ID;
    use dial9_trace_format::encoder::Encoder;
    use dial9_trace_format::{FieldValue, InternedString};
    const UNKNOWN_SPAWN_LOC: InternedString = InternedString::from_raw(0);

    #[test]
    fn trace_reader_reads_gzip_trace_files() {
        use crate::telemetry::buffer::ThreadLocalBuffer;
        use crate::telemetry::format::WorkerParkEvent;
        use crate::telemetry::writer::{RotatingWriter, TraceWriter};
        use flate2::Compression;
        use flate2::write::GzEncoder;
        use std::io::Write;

        let dir = tempfile::tempdir().unwrap();
        let raw_path = dir.path().join("trace.bin");

        let mut writer = RotatingWriter::single_file(&raw_path).unwrap();
        let batch = crate::telemetry::collector::Batch::new(
            ThreadLocalBuffer::encode_single(&WorkerParkEvent {
                timestamp_ns: 1_000,
                worker_id: WorkerId::from(7usize),
                local_queue: 3,
                cpu_time_ns: 11,
                tid: 0,
            }),
            1,
        );
        writer.write_encoded_batch(&batch).unwrap();
        writer.flush().unwrap();
        let active = writer.current_active_path().to_owned();
        drop(writer);

        let raw = std::fs::read(&active).unwrap();
        let gzip_path = dir.path().join("trace.bin.gz");
        let mut encoder = GzEncoder::new(Vec::new(), Compression::fast());
        encoder.write_all(&raw).unwrap();
        let compressed = encoder.finish().unwrap();
        std::fs::write(&gzip_path, compressed).unwrap();

        let reader = TraceReader::new(gzip_path.to_str().unwrap()).unwrap();
        let events = &reader.runtime_events;

        assert_eq!(events.len(), 1);
        assert!(matches!(
            events[0],
            TelemetryEvent::WorkerPark {
                timestamp_nanos: 1_000,
                worker_id,
                worker_local_queue_depth: 3,
                cpu_time_nanos: 11,
                ..
            } if worker_id == WorkerId::from(7usize)
        ));
    }

    #[test]
    fn test_analyze_empty() {
        let events = vec![];
        let analysis = analyze_trace(&events);
        assert_eq!(analysis.total_events, 0);
        assert_eq!(analysis.max_global_queue, 0);
    }

    #[test]
    fn test_global_queue_from_samples_only() {
        let events = vec![
            TelemetryEvent::PollStart {
                timestamp_nanos: 1_000_000,
                worker_id: WorkerId::from(0usize),
                worker_local_queue_depth: 3,
                task_id: UNKNOWN_TASK_ID,
                spawn_loc: UNKNOWN_SPAWN_LOC,
            },
            TelemetryEvent::QueueSample {
                timestamp_nanos: 2_000_000,
                global_queue_depth: 42,
            },
            TelemetryEvent::PollEnd {
                timestamp_nanos: 3_000_000,
                worker_id: WorkerId::from(0usize),
            },
        ];
        let analysis = analyze_trace(&events);
        assert_eq!(analysis.max_global_queue, 42);
        assert_eq!(analysis.total_events, 3);
        // Only the QueueSample contributes to avg
        assert!((analysis.avg_global_queue - 42.0).abs() < f64::EPSILON);
    }

    #[test]
    fn test_local_queue_tracked_on_all_worker_events() {
        let events = vec![
            TelemetryEvent::PollStart {
                timestamp_nanos: 1_000_000,
                worker_id: WorkerId::from(0usize),
                worker_local_queue_depth: 10,
                task_id: UNKNOWN_TASK_ID,
                spawn_loc: UNKNOWN_SPAWN_LOC,
            },
            TelemetryEvent::PollEnd {
                timestamp_nanos: 2_000_000,
                worker_id: WorkerId::from(0usize),
            },
        ];
        let analysis = analyze_trace(&events);
        let stats = analysis.worker_stats.get(&WorkerId::from(0usize)).unwrap();
        assert_eq!(stats.max_local_queue, 10);
    }

    #[test]
    fn test_lookup_global_queue_depth() {
        let timeline = vec![(1_000_000u64, 5), (3_000_000, 10), (5_000_000, 2)];
        // Before any sample
        assert_eq!(lookup_global_queue_depth(&timeline, 500_000), 0);
        // Exact match
        assert_eq!(lookup_global_queue_depth(&timeline, 1_000_000), 5);
        // Between samples — use most recent
        assert_eq!(lookup_global_queue_depth(&timeline, 2_000_000), 5);
        assert_eq!(lookup_global_queue_depth(&timeline, 4_000_000), 10);
        // After last sample
        assert_eq!(lookup_global_queue_depth(&timeline, 9_000_000), 2);
    }

    #[test]
    fn test_detect_idle_workers_with_samples() {
        let events = vec![
            TelemetryEvent::QueueSample {
                timestamp_nanos: 1_000_000,
                global_queue_depth: 15,
            },
            TelemetryEvent::WorkerPark {
                timestamp_nanos: 2_000_000,
                worker_id: WorkerId::from(0usize),
                worker_local_queue_depth: 0,
                cpu_time_nanos: 0,
                tid: 0,
            },
            TelemetryEvent::QueueSample {
                timestamp_nanos: 5_000_000,
                global_queue_depth: 20,
            },
            TelemetryEvent::WorkerUnpark {
                timestamp_nanos: 6_000_000,
                worker_id: WorkerId::from(0usize),
                worker_local_queue_depth: 0,
                cpu_time_nanos: 0,
                sched_wait_delta_nanos: 0,
                tid: 0,
            },
        ];
        let idle = detect_idle_workers(&events);
        assert_eq!(idle.len(), 1);
        assert_eq!(idle[0].0, WorkerId::from(0usize)); // worker_id
        assert_eq!(idle[0].1, 4_000_000); // idle duration
        assert_eq!(idle[0].2, 20); // global queue depth at unpark
    }

    #[test]
    fn test_detect_long_polls_above_threshold() {
        let events = vec![
            TelemetryEvent::PollStart {
                timestamp_nanos: 1_000_000,
                worker_id: WorkerId::from(0usize),
                worker_local_queue_depth: 0,
                task_id: TaskId::from_u32(1),
                spawn_loc: UNKNOWN_SPAWN_LOC,
            },
            TelemetryEvent::PollEnd {
                timestamp_nanos: 3_000_000, // 2ms poll
                worker_id: WorkerId::from(0usize),
            },
            TelemetryEvent::PollStart {
                timestamp_nanos: 4_000_000,
                worker_id: WorkerId::from(0usize),
                worker_local_queue_depth: 0,
                task_id: TaskId::from_u32(2),
                spawn_loc: UNKNOWN_SPAWN_LOC,
            },
            TelemetryEvent::PollEnd {
                timestamp_nanos: 4_500_000, // 0.5ms poll
                worker_id: WorkerId::from(0usize),
            },
        ];
        let long = detect_long_polls(&events, 1_000_000); // 1ms threshold
        assert_eq!(long.len(), 1);
        assert_eq!(long[0].worker_id, WorkerId::from(0usize));
        assert_eq!(long[0].duration_ns, 2_000_000);
        assert_eq!(long[0].task_id, TaskId::from_u32(1));
    }

    #[test]
    fn test_detect_long_polls_none_above_threshold() {
        let events = vec![
            TelemetryEvent::PollStart {
                timestamp_nanos: 1_000_000,
                worker_id: WorkerId::from(0usize),
                worker_local_queue_depth: 0,
                task_id: UNKNOWN_TASK_ID,
                spawn_loc: UNKNOWN_SPAWN_LOC,
            },
            TelemetryEvent::PollEnd {
                timestamp_nanos: 1_500_000, // 0.5ms
                worker_id: WorkerId::from(0usize),
            },
        ];
        let long = detect_long_polls(&events, 1_000_000);
        assert!(long.is_empty());
    }

    #[test]
    fn test_detect_long_polls_multiple_workers() {
        let events = vec![
            TelemetryEvent::PollStart {
                timestamp_nanos: 1_000_000,
                worker_id: WorkerId::from(0usize),
                worker_local_queue_depth: 0,
                task_id: TaskId::from_u32(1),
                spawn_loc: UNKNOWN_SPAWN_LOC,
            },
            TelemetryEvent::PollStart {
                timestamp_nanos: 1_000_000,
                worker_id: WorkerId::from(1usize),
                worker_local_queue_depth: 0,
                task_id: TaskId::from_u32(2),
                spawn_loc: UNKNOWN_SPAWN_LOC,
            },
            TelemetryEvent::PollEnd {
                timestamp_nanos: 5_000_000, // 4ms
                worker_id: WorkerId::from(0usize),
            },
            TelemetryEvent::PollEnd {
                timestamp_nanos: 8_000_000, // 7ms
                worker_id: WorkerId::from(1usize),
            },
        ];
        let long = detect_long_polls(&events, 1_000_000);
        assert_eq!(long.len(), 2);
        assert_eq!(long[0].worker_id, WorkerId::from(0usize));
        assert_eq!(long[0].duration_ns, 4_000_000);
        assert_eq!(long[1].worker_id, WorkerId::from(1usize));
        assert_eq!(long[1].duration_ns, 7_000_000);
    }

    #[test]
    fn test_detect_sched_delays_above_threshold() {
        let events = vec![
            TelemetryEvent::WorkerPark {
                timestamp_nanos: 1_000_000,
                worker_id: WorkerId::from(0usize),
                worker_local_queue_depth: 0,
                cpu_time_nanos: 0,
                tid: 0,
            },
            TelemetryEvent::WorkerUnpark {
                timestamp_nanos: 5_000_000,
                worker_id: WorkerId::from(0usize),
                worker_local_queue_depth: 0,
                cpu_time_nanos: 0,
                sched_wait_delta_nanos: 200_000, // 200us
                tid: 0,
            },
        ];
        let delays = detect_sched_delays(&events, 100_000); // 100us threshold
        assert_eq!(delays.len(), 1);
        assert_eq!(delays[0].worker_id, WorkerId::from(0usize));
        assert_eq!(delays[0].sched_wait_ns, 200_000);
        assert_eq!(delays[0].park_ns, 1_000_000);
        assert_eq!(delays[0].unpark_ns, 5_000_000);
    }

    #[test]
    fn test_detect_sched_delays_below_threshold() {
        let events = vec![
            TelemetryEvent::WorkerPark {
                timestamp_nanos: 1_000_000,
                worker_id: WorkerId::from(0usize),
                worker_local_queue_depth: 0,
                cpu_time_nanos: 0,
                tid: 0,
            },
            TelemetryEvent::WorkerUnpark {
                timestamp_nanos: 2_000_000,
                worker_id: WorkerId::from(0usize),
                worker_local_queue_depth: 0,
                cpu_time_nanos: 0,
                sched_wait_delta_nanos: 50_000, // 50us
                tid: 0,
            },
        ];
        let delays = detect_sched_delays(&events, 100_000);
        assert!(delays.is_empty());
    }

    #[test]
    fn test_detect_sched_delays_multiple_workers() {
        let events = vec![
            TelemetryEvent::WorkerPark {
                timestamp_nanos: 1_000_000,
                worker_id: WorkerId::from(0usize),
                worker_local_queue_depth: 0,
                cpu_time_nanos: 0,
                tid: 0,
            },
            TelemetryEvent::WorkerPark {
                timestamp_nanos: 1_000_000,
                worker_id: WorkerId::from(1usize),
                worker_local_queue_depth: 0,
                cpu_time_nanos: 0,
                tid: 0,
            },
            TelemetryEvent::WorkerUnpark {
                timestamp_nanos: 3_000_000,
                worker_id: WorkerId::from(0usize),
                worker_local_queue_depth: 0,
                cpu_time_nanos: 0,
                sched_wait_delta_nanos: 500_000, // 500us
                tid: 0,
            },
            TelemetryEvent::WorkerUnpark {
                timestamp_nanos: 4_000_000,
                worker_id: WorkerId::from(1usize),
                worker_local_queue_depth: 0,
                cpu_time_nanos: 0,
                sched_wait_delta_nanos: 10_000, // 10us - below threshold
                tid: 0,
            },
        ];
        let delays = detect_sched_delays(&events, 100_000);
        assert_eq!(delays.len(), 1);
        assert_eq!(delays[0].worker_id, WorkerId::from(0usize));
    }

    #[test]
    fn test_detect_wake_delays_above_threshold() {
        let task = TaskId::from_u32(1);
        let events = vec![
            TelemetryEvent::WakeEvent {
                timestamp_nanos: 1_000_000,
                waker_task_id: UNKNOWN_TASK_ID,
                woken_task_id: task,
                target_worker: 0,
            },
            TelemetryEvent::PollStart {
                timestamp_nanos: 1_500_000, // 500us delay
                worker_id: WorkerId::from(0usize),
                worker_local_queue_depth: 0,
                task_id: task,
                spawn_loc: UNKNOWN_SPAWN_LOC,
            },
            TelemetryEvent::PollEnd {
                timestamp_nanos: 1_600_000,
                worker_id: WorkerId::from(0usize),
            },
        ];
        let delays = detect_wake_delays(&events, 100_000); // 100us threshold
        assert_eq!(delays.len(), 1);
        assert_eq!(delays[0].delay_ns, 500_000);
        assert_eq!(delays[0].task_id, task);
        assert_eq!(delays[0].worker_id, WorkerId::from(0usize));
    }

    #[test]
    fn test_detect_wake_delays_below_threshold() {
        let task = TaskId::from_u32(1);
        let events = vec![
            TelemetryEvent::WakeEvent {
                timestamp_nanos: 1_000_000,
                waker_task_id: UNKNOWN_TASK_ID,
                woken_task_id: task,
                target_worker: 0,
            },
            TelemetryEvent::PollStart {
                timestamp_nanos: 1_050_000, // 50us delay
                worker_id: WorkerId::from(0usize),
                worker_local_queue_depth: 0,
                task_id: task,
                spawn_loc: UNKNOWN_SPAWN_LOC,
            },
            TelemetryEvent::PollEnd {
                timestamp_nanos: 1_100_000,
                worker_id: WorkerId::from(0usize),
            },
        ];
        let delays = detect_wake_delays(&events, 100_000);
        assert!(delays.is_empty());
    }

    #[test]
    fn test_detect_wake_delays_discards_idle_tasks() {
        let task = TaskId::from_u32(1);
        let events = vec![
            TelemetryEvent::WakeEvent {
                timestamp_nanos: 1_000_000,
                waker_task_id: UNKNOWN_TASK_ID,
                woken_task_id: task,
                target_worker: 0,
            },
            TelemetryEvent::PollStart {
                timestamp_nanos: 2_000_000_000, // over 1s cap
                worker_id: WorkerId::from(0usize),
                worker_local_queue_depth: 0,
                task_id: task,
                spawn_loc: UNKNOWN_SPAWN_LOC,
            },
            TelemetryEvent::PollEnd {
                timestamp_nanos: 2_000_100_000,
                worker_id: WorkerId::from(0usize),
            },
        ];
        let delays = detect_wake_delays(&events, 100_000);
        assert!(delays.is_empty());
    }

    #[test]
    fn test_detect_sampled_polls_with_samples() {
        let events = vec![
            TelemetryEvent::PollStart {
                timestamp_nanos: 1_000_000,
                worker_id: WorkerId::from(0usize),
                worker_local_queue_depth: 0,
                task_id: TaskId::from_u32(1),
                spawn_loc: UNKNOWN_SPAWN_LOC,
            },
            TelemetryEvent::CpuSample {
                timestamp_nanos: 1_500_000,
                worker_id: WorkerId::from(0usize),
                tid: 100,
                thread_name: None,
                source: CpuSampleSource::CpuProfile,
                callchain: vec![],
                cpu: None,
            },
            TelemetryEvent::CpuSample {
                timestamp_nanos: 1_800_000,
                worker_id: WorkerId::from(0usize),
                tid: 100,
                thread_name: None,
                source: CpuSampleSource::SchedEvent,
                callchain: vec![],
                cpu: None,
            },
            TelemetryEvent::PollEnd {
                timestamp_nanos: 2_000_000,
                worker_id: WorkerId::from(0usize),
            },
        ];
        let sampled = detect_sampled_polls(&events);
        assert_eq!(sampled.len(), 1);
        assert_eq!(sampled[0].cpu_sample_count, 1);
        assert_eq!(sampled[0].sched_sample_count, 1);
        assert_eq!(sampled[0].task_id, TaskId::from_u32(1));
    }

    #[test]
    fn test_detect_sampled_polls_no_samples() {
        let events = vec![
            TelemetryEvent::PollStart {
                timestamp_nanos: 1_000_000,
                worker_id: WorkerId::from(0usize),
                worker_local_queue_depth: 0,
                task_id: UNKNOWN_TASK_ID,
                spawn_loc: UNKNOWN_SPAWN_LOC,
            },
            TelemetryEvent::PollEnd {
                timestamp_nanos: 2_000_000,
                worker_id: WorkerId::from(0usize),
            },
        ];
        let sampled = detect_sampled_polls(&events);
        assert!(sampled.is_empty());
    }

    #[test]
    fn test_detect_sampled_polls_sample_outside_poll() {
        let events = vec![
            TelemetryEvent::PollStart {
                timestamp_nanos: 1_000_000,
                worker_id: WorkerId::from(0usize),
                worker_local_queue_depth: 0,
                task_id: UNKNOWN_TASK_ID,
                spawn_loc: UNKNOWN_SPAWN_LOC,
            },
            TelemetryEvent::PollEnd {
                timestamp_nanos: 2_000_000,
                worker_id: WorkerId::from(0usize),
            },
            TelemetryEvent::CpuSample {
                timestamp_nanos: 3_000_000, // after poll ended
                worker_id: WorkerId::from(0usize),
                tid: 100,
                thread_name: None,
                source: CpuSampleSource::CpuProfile,
                callchain: vec![],
                cpu: None,
            },
        ];
        let sampled = detect_sampled_polls(&events);
        assert!(sampled.is_empty());
    }

    #[test]
    fn test_detect_idle_workers_no_queue_pressure() {
        let events = vec![
            TelemetryEvent::QueueSample {
                timestamp_nanos: 1_000_000,
                global_queue_depth: 0, // no queue pressure
            },
            TelemetryEvent::WorkerPark {
                timestamp_nanos: 2_000_000,
                worker_id: WorkerId::from(0usize),
                worker_local_queue_depth: 0,
                cpu_time_nanos: 0,
                tid: 0,
            },
            TelemetryEvent::WorkerUnpark {
                timestamp_nanos: 6_000_000,
                worker_id: WorkerId::from(0usize),
                worker_local_queue_depth: 0,
                cpu_time_nanos: 0,
                sched_wait_delta_nanos: 0,
                tid: 0,
            },
        ];
        let idle = detect_idle_workers(&events);
        assert!(idle.is_empty()); // no idle periods flagged because global queue was empty
    }

    #[test]
    fn trace_reader_custom_events_resolve_interned_at_parse_time() {
        // A custom event type with an interned string field.
        #[derive(dial9_trace_format::TraceEvent)]
        struct MyEvent {
            #[traceevent(timestamp)]
            timestamp_ns: u64,
            label: InternedString,
            count: u32,
        }

        // Helper: build a batch with one event whose label is an interned string.
        fn make_batch(ts: u64, label: &str, count: u32) -> Vec<u8> {
            let mut enc = Encoder::new();
            let s = enc.intern_string_infallible(label);
            enc.write(&MyEvent {
                timestamp_ns: ts,
                label: s,
                count,
            })
            .unwrap();
            enc.reset_to_infallible(Vec::new())
        }

        // Helper: extract resolved label strings from Custom events.
        fn custom_labels(reader: &TraceReader) -> Vec<String> {
            reader
                .all_events
                .iter()
                .filter_map(|ev| {
                    if let TelemetryEvent::Custom { fields, .. } = ev
                        && let FieldValue::String(s) = &fields[0].1
                    {
                        return Some(s.clone());
                    }
                    None
                })
                .collect()
        }

        // Three segments with different strings, same pool size (1 entry each).
        // All three get raw ID 0 since each encoder starts fresh, but
        // PooledString is resolved to String at parse time.
        let mut output = Encoder::new().finish();
        output.extend_from_slice(&make_batch(1_000, "alpha", 1));
        output.extend_from_slice(&make_batch(2_000, "beta", 2));
        output.extend_from_slice(&make_batch(3_000, "gamma", 3));

        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("trace.bin");
        std::fs::write(&path, &output).unwrap();

        let reader = TraceReader::new(path.to_str().unwrap()).unwrap();
        let labels = custom_labels(&reader);
        assert_eq!(labels, vec!["alpha", "beta", "gamma"]);

        // Colliding first value: two segments both intern "shared" as their
        // first string (same raw ID), plus a second string that differs.
        let mut output2 = Encoder::new().finish();
        {
            let mut enc = Encoder::new();
            let shared = enc.intern_string_infallible("shared");
            let unique = enc.intern_string_infallible("seg1_only");
            enc.write(&MyEvent {
                timestamp_ns: 10_000,
                label: shared,
                count: 1,
            })
            .unwrap();
            enc.write(&MyEvent {
                timestamp_ns: 11_000,
                label: unique,
                count: 2,
            })
            .unwrap();
            output2.extend_from_slice(&enc.reset_to_infallible(Vec::new()));
        }
        {
            let mut enc = Encoder::new();
            let shared = enc.intern_string_infallible("shared");
            let unique = enc.intern_string_infallible("seg2_only");
            enc.write(&MyEvent {
                timestamp_ns: 20_000,
                label: shared,
                count: 3,
            })
            .unwrap();
            enc.write(&MyEvent {
                timestamp_ns: 21_000,
                label: unique,
                count: 4,
            })
            .unwrap();
            output2.extend_from_slice(&enc.reset_to_infallible(Vec::new()));
        }

        let path2 = dir.path().join("trace2.bin");
        std::fs::write(&path2, &output2).unwrap();
        let reader2 = TraceReader::new(path2.to_str().unwrap()).unwrap();
        let labels2 = custom_labels(&reader2);
        assert_eq!(labels2, vec!["shared", "seg1_only", "shared", "seg2_only"]);
    }

    #[test]
    fn trace_reader_custom_event_primitive_fields_only() {
        #[derive(dial9_trace_format::TraceEvent)]
        struct CounterEvent {
            #[traceevent(timestamp)]
            timestamp_ns: u64,
            count: u32,
            rate: u64,
        }

        let mut enc = Encoder::new();
        enc.write(&CounterEvent {
            timestamp_ns: 5_000,
            count: 42,
            rate: 1_000_000,
        })
        .unwrap();

        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("trace.bin");
        std::fs::write(&path, enc.finish()).unwrap();

        let reader = TraceReader::new(path.to_str().unwrap()).unwrap();
        let custom: Vec<&TelemetryEvent> = reader
            .all_events
            .iter()
            .filter(|e| matches!(e, TelemetryEvent::Custom { .. }))
            .collect();
        assert_eq!(custom.len(), 1);

        if let TelemetryEvent::Custom {
            name,
            timestamp_nanos,
            fields,
        } = &custom[0]
        {
            assert_eq!(name, "CounterEvent");
            assert_eq!(*timestamp_nanos, Some(5_000));
            assert_eq!(fields.len(), 2);
            assert_eq!(fields[0], ("count".to_string(), FieldValue::Varint(42)));
            assert_eq!(
                fields[1],
                ("rate".to_string(), FieldValue::Varint(1_000_000))
            );
        } else {
            panic!("expected Custom");
        }
    }

    #[test]
    fn trace_reader_custom_event_inline_string_fields() {
        #[derive(dial9_trace_format::TraceEvent)]
        struct LogEvent {
            #[traceevent(timestamp)]
            timestamp_ns: u64,
            message: String,
            level: u32,
        }

        let mut enc = Encoder::new();
        enc.write(&LogEvent {
            timestamp_ns: 7_000,
            message: "request handled".to_string(),
            level: 2,
        })
        .unwrap();

        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("trace.bin");
        std::fs::write(&path, enc.finish()).unwrap();

        let reader = TraceReader::new(path.to_str().unwrap()).unwrap();
        let custom: Vec<&TelemetryEvent> = reader
            .all_events
            .iter()
            .filter(|e| matches!(e, TelemetryEvent::Custom { .. }))
            .collect();
        assert_eq!(custom.len(), 1);

        if let TelemetryEvent::Custom {
            name,
            timestamp_nanos,
            fields,
        } = &custom[0]
        {
            assert_eq!(name, "LogEvent");
            assert_eq!(*timestamp_nanos, Some(7_000));
            assert_eq!(fields.len(), 2);
            assert_eq!(
                fields[0],
                (
                    "message".to_string(),
                    FieldValue::String("request handled".to_string())
                )
            );
            assert_eq!(fields[1], ("level".to_string(), FieldValue::Varint(2)));
        } else {
            panic!("expected Custom");
        }
    }

    #[test]
    fn trace_reader_custom_event_interned_string_resolved_eagerly() {
        #[derive(dial9_trace_format::TraceEvent)]
        struct MixedEvent {
            #[traceevent(timestamp)]
            timestamp_ns: u64,
            tag: InternedString,
            count: u32,
        }

        let mut enc = Encoder::new();
        let tag = enc.intern_string_infallible("important");
        enc.write(&MixedEvent {
            timestamp_ns: 1_000,
            tag,
            count: 7,
        })
        .unwrap();

        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("trace.bin");
        std::fs::write(&path, enc.finish()).unwrap();

        let reader = TraceReader::new(path.to_str().unwrap()).unwrap();
        let event = reader
            .all_events
            .iter()
            .find(|e| matches!(e, TelemetryEvent::Custom { .. }))
            .expect("should have a custom event");

        if let TelemetryEvent::Custom { fields, .. } = event {
            // PooledString resolved to String at parse time
            assert_eq!(fields[0].0, "tag");
            assert_eq!(fields[0].1, FieldValue::String("important".to_string()));
            assert_eq!(fields[1].0, "count");
            assert_eq!(fields[1].1, FieldValue::Varint(7));
        } else {
            panic!("expected Custom");
        }
    }
}
