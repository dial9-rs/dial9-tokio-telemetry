use crate::telemetry::events::{CpuSampleSource, TelemetryEvent};
use crate::telemetry::format;
use crate::telemetry::task_metadata::{SpawnLocationId, TaskId};
use std::collections::HashMap;
use std::fs::File;
use std::io::{BufReader, Result};

pub struct TraceReader {
    reader: BufReader<File>,
    /// Spawn location definitions accumulated during reading.
    pub spawn_locations: HashMap<SpawnLocationId, String>,
    /// Task ID → spawn location ID mapping built from TaskSpawn events.
    pub task_spawn_locs: HashMap<TaskId, SpawnLocationId>,
    /// Callframe address → symbol name mapping built from CallframeDef events.
    pub callframe_symbols: HashMap<u64, String>,
    /// OS tid → thread name mapping built from ThreadNameDef events.
    pub thread_names: HashMap<u32, String>,
    /// Key-value metadata from the most recent SegmentMetadata event.
    pub segment_metadata: Vec<(String, String)>,
}

impl TraceReader {
    pub fn new(path: &str) -> Result<Self> {
        let file = File::open(path)?;
        Ok(Self {
            reader: BufReader::new(file),
            spawn_locations: HashMap::new(),
            task_spawn_locs: HashMap::new(),
            callframe_symbols: HashMap::new(),
            thread_names: HashMap::new(),
            segment_metadata: Vec::new(),
        })
    }

    pub fn read_header(&mut self) -> Result<(String, u32)> {
        format::read_header(&mut self.reader)
    }

    /// Read the next event without filtering — returns all events including
    /// metadata records (SpawnLocationDef, TaskSpawn, CallframeDef).
    /// Still accumulates metadata into lookup tables as a side effect.
    pub fn read_raw_event(&mut self) -> Result<Option<TelemetryEvent>> {
        let event = format::read_event(&mut self.reader)?;
        match &event {
            Some(TelemetryEvent::SpawnLocationDef { id, location }) => {
                self.spawn_locations.insert(*id, location.clone());
            }
            Some(TelemetryEvent::TaskSpawn {
                task_id,
                spawn_loc_id,
                ..
            }) => {
                self.task_spawn_locs.insert(*task_id, *spawn_loc_id);
            }
            Some(TelemetryEvent::CallframeDef {
                address, symbol, ..
            }) => {
                self.callframe_symbols.insert(*address, symbol.clone());
            }
            Some(TelemetryEvent::ThreadNameDef { tid, name }) => {
                self.thread_names.insert(*tid, name.clone());
            }
            Some(TelemetryEvent::SegmentMetadata { entries }) => {
                self.segment_metadata = entries.clone();
            }
            _ => {}
        }
        Ok(event)
    }

    /// Read the next runtime telemetry event, automatically accumulating
    /// SpawnLocationDef and TaskSpawn metadata records.
    pub fn read_event(&mut self) -> Result<Option<TelemetryEvent>> {
        loop {
            match format::read_event(&mut self.reader)? {
                None => return Ok(None),
                Some(TelemetryEvent::SpawnLocationDef { id, location }) => {
                    self.spawn_locations.insert(id, location);
                }
                Some(TelemetryEvent::TaskSpawn {
                    task_id,
                    spawn_loc_id,
                    ..
                }) => {
                    self.task_spawn_locs.insert(task_id, spawn_loc_id);
                }
                Some(TelemetryEvent::CallframeDef {
                    address, symbol, ..
                }) => {
                    self.callframe_symbols.insert(address, symbol);
                }
                Some(TelemetryEvent::ThreadNameDef { tid, name }) => {
                    self.thread_names.insert(tid, name);
                }
                Some(TelemetryEvent::SegmentMetadata { entries }) => {
                    self.segment_metadata = entries;
                }
                Some(e) => return Ok(Some(e)),
            }
        }
    }

    pub fn read_all(&mut self) -> Result<Vec<TelemetryEvent>> {
        let mut events = Vec::new();
        while let Some(event) = self.read_event()? {
            events.push(event);
        }
        Ok(events)
    }
}

#[derive(Debug, Default)]
pub struct WorkerStats {
    pub poll_count: usize,
    pub park_count: usize,
    pub unpark_count: usize,
    pub total_poll_time_ns: u64,
    pub max_local_queue: usize,
    pub max_sched_wait_ns: u64,
    pub total_sched_wait_ns: u64,
}

#[derive(Debug, Default)]
pub struct SpawnLocationStats {
    pub poll_count: usize,
    pub total_poll_time_ns: u64,
}

#[derive(Debug)]
pub struct TraceAnalysis {
    pub total_events: usize,
    pub duration_ns: u64,
    pub worker_stats: HashMap<usize, WorkerStats>,
    pub max_global_queue: usize,
    pub avg_global_queue: f64,
    /// Per-spawn-location statistics (only populated when task tracking is enabled).
    pub spawn_location_stats: HashMap<SpawnLocationId, SpawnLocationStats>,
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

pub fn analyze_trace(events: &[TelemetryEvent]) -> TraceAnalysis {
    let mut worker_stats: HashMap<usize, WorkerStats> = HashMap::new();
    let mut poll_starts: HashMap<usize, u64> = HashMap::new();
    // Track spawn_loc_id at PollStart for computing per-location poll time at PollEnd
    let mut poll_start_locs: HashMap<usize, SpawnLocationId> = HashMap::new();
    let mut spawn_location_stats: HashMap<SpawnLocationId, SpawnLocationStats> = HashMap::new();
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
                spawn_loc_id,
                ..
            } => {
                let stats = worker_stats.entry(*worker_id).or_default();
                stats.max_local_queue = stats.max_local_queue.max(*worker_local_queue_depth);
                stats.poll_count += 1;
                poll_starts.insert(*worker_id, *timestamp_nanos);
                if spawn_loc_id.as_u16() != 0 {
                    spawn_location_stats
                        .entry(*spawn_loc_id)
                        .or_default()
                        .poll_count += 1;
                    poll_start_locs.insert(*worker_id, *spawn_loc_id);
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
            TelemetryEvent::SpawnLocationDef { .. }
            | TelemetryEvent::TaskSpawn { .. }
            | TelemetryEvent::TaskTerminate { .. }
            | TelemetryEvent::CpuSample { .. }
            | TelemetryEvent::CallframeDef { .. }
            | TelemetryEvent::ThreadNameDef { .. }
            | TelemetryEvent::WakeEvent { .. }
            | TelemetryEvent::SegmentMetadata { .. } => {}
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
    pub worker_id: usize,
    pub start_ns: u64,
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
    let mut unpark_state: HashMap<usize, (u64, u64)> = HashMap::new();

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

pub fn print_analysis(
    analysis: &TraceAnalysis,
    spawn_locations: &HashMap<SpawnLocationId, String>,
) {
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
        locs.sort_by(|a, b| b.1.poll_count.cmp(&a.1.poll_count));
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
pub fn detect_idle_workers(events: &[TelemetryEvent]) -> Vec<(usize, u64, usize)> {
    let global_queue_timeline = build_global_queue_timeline(events);
    let mut idle_periods = Vec::new();
    let mut worker_park_times: HashMap<usize, u64> = HashMap::new();

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
    pub worker_id: usize,
    pub start_ns: u64,
    pub end_ns: u64,
    pub duration_ns: u64,
    pub task_id: TaskId,
    pub spawn_loc_id: SpawnLocationId,
}

/// Detect polls that exceed `threshold_ns` nanoseconds.
///
/// Returns long polls in timestamp order. Each entry captures the worker,
/// time range, and task metadata (when task tracking is enabled).
pub fn detect_long_polls(events: &[TelemetryEvent], threshold_ns: u64) -> Vec<LongPoll> {
    let mut long_polls = Vec::new();
    let mut poll_starts: HashMap<usize, (u64, TaskId, SpawnLocationId)> = HashMap::new();

    for event in events {
        match event {
            TelemetryEvent::PollStart {
                timestamp_nanos,
                worker_id,
                task_id,
                spawn_loc_id,
                ..
            } => {
                poll_starts.insert(*worker_id, (*timestamp_nanos, *task_id, *spawn_loc_id));
            }
            TelemetryEvent::PollEnd {
                timestamp_nanos,
                worker_id,
            } => {
                if let Some((start, task_id, spawn_loc_id)) = poll_starts.remove(worker_id) {
                    let duration = timestamp_nanos.saturating_sub(start);
                    if duration >= threshold_ns {
                        long_polls.push(LongPoll {
                            worker_id: *worker_id,
                            start_ns: start,
                            end_ns: *timestamp_nanos,
                            duration_ns: duration,
                            task_id,
                            spawn_loc_id,
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
    pub worker_id: usize,
    pub park_ns: u64,
    pub unpark_ns: u64,
    pub sched_wait_ns: u64,
}

/// Detect park periods where OS scheduling wait exceeded `threshold_ns`.
///
/// `sched_wait_delta_nanos` on `WorkerUnpark` reports how long the thread was
/// runnable but not scheduled by the OS during the preceding park.
pub fn detect_sched_delays(events: &[TelemetryEvent], threshold_ns: u64) -> Vec<SchedDelay> {
    let mut delays = Vec::new();
    let mut park_times: HashMap<usize, u64> = HashMap::new();

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
    pub worker_id: usize,
    pub wake_ns: u64,
    pub poll_ns: u64,
    pub delay_ns: u64,
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
    pub worker_id: usize,
    pub start_ns: u64,
    pub end_ns: u64,
    pub task_id: TaskId,
    pub spawn_loc_id: SpawnLocationId,
    pub cpu_sample_count: usize,
    pub sched_sample_count: usize,
}

/// Find polls that had CPU profile or scheduler event samples collected during
/// their execution. Correlates `CpuSample` events with poll time ranges on the
/// same worker.
pub fn detect_sampled_polls(events: &[TelemetryEvent]) -> Vec<SampledPoll> {
    struct PollSpan {
        worker_id: usize,
        start_ns: u64,
        end_ns: u64,
        task_id: TaskId,
        spawn_loc_id: SpawnLocationId,
        cpu_samples: usize,
        sched_samples: usize,
    }

    // First pass: build poll spans per worker
    let mut polls: Vec<PollSpan> = Vec::new();
    let mut poll_starts: HashMap<usize, (u64, TaskId, SpawnLocationId)> = HashMap::new();

    for event in events {
        match event {
            TelemetryEvent::PollStart {
                timestamp_nanos,
                worker_id,
                task_id,
                spawn_loc_id,
                ..
            } => {
                poll_starts.insert(*worker_id, (*timestamp_nanos, *task_id, *spawn_loc_id));
            }
            TelemetryEvent::PollEnd {
                timestamp_nanos,
                worker_id,
            } => {
                if let Some((start, task_id, spawn_loc_id)) = poll_starts.remove(worker_id) {
                    polls.push(PollSpan {
                        worker_id: *worker_id,
                        start_ns: start,
                        end_ns: *timestamp_nanos,
                        task_id,
                        spawn_loc_id,
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
            spawn_loc_id: p.spawn_loc_id,
            cpu_sample_count: p.cpu_samples,
            sched_sample_count: p.sched_samples,
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::telemetry::task_metadata::{UNKNOWN_SPAWN_LOCATION_ID, UNKNOWN_TASK_ID};

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
                worker_id: 0,
                worker_local_queue_depth: 3,
                task_id: UNKNOWN_TASK_ID,
                spawn_loc_id: UNKNOWN_SPAWN_LOCATION_ID,
            },
            TelemetryEvent::QueueSample {
                timestamp_nanos: 2_000_000,
                global_queue_depth: 42,
            },
            TelemetryEvent::PollEnd {
                timestamp_nanos: 3_000_000,
                worker_id: 0,
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
                worker_id: 0,
                worker_local_queue_depth: 10,
                task_id: UNKNOWN_TASK_ID,
                spawn_loc_id: UNKNOWN_SPAWN_LOCATION_ID,
            },
            TelemetryEvent::PollEnd {
                timestamp_nanos: 2_000_000,
                worker_id: 0,
            },
        ];
        let analysis = analyze_trace(&events);
        let stats = analysis.worker_stats.get(&0).unwrap();
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
                worker_id: 0,
                worker_local_queue_depth: 0,
                cpu_time_nanos: 0,
            },
            TelemetryEvent::QueueSample {
                timestamp_nanos: 5_000_000,
                global_queue_depth: 20,
            },
            TelemetryEvent::WorkerUnpark {
                timestamp_nanos: 6_000_000,
                worker_id: 0,
                worker_local_queue_depth: 0,
                cpu_time_nanos: 0,
                sched_wait_delta_nanos: 0,
            },
        ];
        let idle = detect_idle_workers(&events);
        assert_eq!(idle.len(), 1);
        assert_eq!(idle[0].0, 0); // worker_id
        assert_eq!(idle[0].1, 4_000_000); // idle duration
        assert_eq!(idle[0].2, 20); // global queue depth at unpark
    }

    #[test]
    fn test_detect_long_polls_above_threshold() {
        let events = vec![
            TelemetryEvent::PollStart {
                timestamp_nanos: 1_000_000,
                worker_id: 0,
                worker_local_queue_depth: 0,
                task_id: TaskId::from_u32(1),
                spawn_loc_id: UNKNOWN_SPAWN_LOCATION_ID,
            },
            TelemetryEvent::PollEnd {
                timestamp_nanos: 3_000_000, // 2ms poll
                worker_id: 0,
            },
            TelemetryEvent::PollStart {
                timestamp_nanos: 4_000_000,
                worker_id: 0,
                worker_local_queue_depth: 0,
                task_id: TaskId::from_u32(2),
                spawn_loc_id: UNKNOWN_SPAWN_LOCATION_ID,
            },
            TelemetryEvent::PollEnd {
                timestamp_nanos: 4_500_000, // 0.5ms poll
                worker_id: 0,
            },
        ];
        let long = detect_long_polls(&events, 1_000_000); // 1ms threshold
        assert_eq!(long.len(), 1);
        assert_eq!(long[0].worker_id, 0);
        assert_eq!(long[0].duration_ns, 2_000_000);
        assert_eq!(long[0].task_id, TaskId::from_u32(1));
    }

    #[test]
    fn test_detect_long_polls_none_above_threshold() {
        let events = vec![
            TelemetryEvent::PollStart {
                timestamp_nanos: 1_000_000,
                worker_id: 0,
                worker_local_queue_depth: 0,
                task_id: UNKNOWN_TASK_ID,
                spawn_loc_id: UNKNOWN_SPAWN_LOCATION_ID,
            },
            TelemetryEvent::PollEnd {
                timestamp_nanos: 1_500_000, // 0.5ms
                worker_id: 0,
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
                worker_id: 0,
                worker_local_queue_depth: 0,
                task_id: TaskId::from_u32(1),
                spawn_loc_id: UNKNOWN_SPAWN_LOCATION_ID,
            },
            TelemetryEvent::PollStart {
                timestamp_nanos: 1_000_000,
                worker_id: 1,
                worker_local_queue_depth: 0,
                task_id: TaskId::from_u32(2),
                spawn_loc_id: UNKNOWN_SPAWN_LOCATION_ID,
            },
            TelemetryEvent::PollEnd {
                timestamp_nanos: 5_000_000, // 4ms
                worker_id: 0,
            },
            TelemetryEvent::PollEnd {
                timestamp_nanos: 8_000_000, // 7ms
                worker_id: 1,
            },
        ];
        let long = detect_long_polls(&events, 1_000_000);
        assert_eq!(long.len(), 2);
        assert_eq!(long[0].worker_id, 0);
        assert_eq!(long[0].duration_ns, 4_000_000);
        assert_eq!(long[1].worker_id, 1);
        assert_eq!(long[1].duration_ns, 7_000_000);
    }

    #[test]
    fn test_detect_sched_delays_above_threshold() {
        let events = vec![
            TelemetryEvent::WorkerPark {
                timestamp_nanos: 1_000_000,
                worker_id: 0,
                worker_local_queue_depth: 0,
                cpu_time_nanos: 0,
            },
            TelemetryEvent::WorkerUnpark {
                timestamp_nanos: 5_000_000,
                worker_id: 0,
                worker_local_queue_depth: 0,
                cpu_time_nanos: 0,
                sched_wait_delta_nanos: 200_000, // 200us
            },
        ];
        let delays = detect_sched_delays(&events, 100_000); // 100us threshold
        assert_eq!(delays.len(), 1);
        assert_eq!(delays[0].worker_id, 0);
        assert_eq!(delays[0].sched_wait_ns, 200_000);
        assert_eq!(delays[0].park_ns, 1_000_000);
        assert_eq!(delays[0].unpark_ns, 5_000_000);
    }

    #[test]
    fn test_detect_sched_delays_below_threshold() {
        let events = vec![
            TelemetryEvent::WorkerPark {
                timestamp_nanos: 1_000_000,
                worker_id: 0,
                worker_local_queue_depth: 0,
                cpu_time_nanos: 0,
            },
            TelemetryEvent::WorkerUnpark {
                timestamp_nanos: 2_000_000,
                worker_id: 0,
                worker_local_queue_depth: 0,
                cpu_time_nanos: 0,
                sched_wait_delta_nanos: 50_000, // 50us
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
                worker_id: 0,
                worker_local_queue_depth: 0,
                cpu_time_nanos: 0,
            },
            TelemetryEvent::WorkerPark {
                timestamp_nanos: 1_000_000,
                worker_id: 1,
                worker_local_queue_depth: 0,
                cpu_time_nanos: 0,
            },
            TelemetryEvent::WorkerUnpark {
                timestamp_nanos: 3_000_000,
                worker_id: 0,
                worker_local_queue_depth: 0,
                cpu_time_nanos: 0,
                sched_wait_delta_nanos: 500_000, // 500us
            },
            TelemetryEvent::WorkerUnpark {
                timestamp_nanos: 4_000_000,
                worker_id: 1,
                worker_local_queue_depth: 0,
                cpu_time_nanos: 0,
                sched_wait_delta_nanos: 10_000, // 10us - below threshold
            },
        ];
        let delays = detect_sched_delays(&events, 100_000);
        assert_eq!(delays.len(), 1);
        assert_eq!(delays[0].worker_id, 0);
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
                worker_id: 0,
                worker_local_queue_depth: 0,
                task_id: task,
                spawn_loc_id: UNKNOWN_SPAWN_LOCATION_ID,
            },
            TelemetryEvent::PollEnd {
                timestamp_nanos: 1_600_000,
                worker_id: 0,
            },
        ];
        let delays = detect_wake_delays(&events, 100_000); // 100us threshold
        assert_eq!(delays.len(), 1);
        assert_eq!(delays[0].delay_ns, 500_000);
        assert_eq!(delays[0].task_id, task);
        assert_eq!(delays[0].worker_id, 0);
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
                worker_id: 0,
                worker_local_queue_depth: 0,
                task_id: task,
                spawn_loc_id: UNKNOWN_SPAWN_LOCATION_ID,
            },
            TelemetryEvent::PollEnd {
                timestamp_nanos: 1_100_000,
                worker_id: 0,
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
                worker_id: 0,
                worker_local_queue_depth: 0,
                task_id: task,
                spawn_loc_id: UNKNOWN_SPAWN_LOCATION_ID,
            },
            TelemetryEvent::PollEnd {
                timestamp_nanos: 2_000_100_000,
                worker_id: 0,
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
                worker_id: 0,
                worker_local_queue_depth: 0,
                task_id: TaskId::from_u32(1),
                spawn_loc_id: UNKNOWN_SPAWN_LOCATION_ID,
            },
            TelemetryEvent::CpuSample {
                timestamp_nanos: 1_500_000,
                worker_id: 0,
                tid: 100,
                source: CpuSampleSource::CpuProfile,
                callchain: vec![],
            },
            TelemetryEvent::CpuSample {
                timestamp_nanos: 1_800_000,
                worker_id: 0,
                tid: 100,
                source: CpuSampleSource::SchedEvent,
                callchain: vec![],
            },
            TelemetryEvent::PollEnd {
                timestamp_nanos: 2_000_000,
                worker_id: 0,
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
                worker_id: 0,
                worker_local_queue_depth: 0,
                task_id: UNKNOWN_TASK_ID,
                spawn_loc_id: UNKNOWN_SPAWN_LOCATION_ID,
            },
            TelemetryEvent::PollEnd {
                timestamp_nanos: 2_000_000,
                worker_id: 0,
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
                worker_id: 0,
                worker_local_queue_depth: 0,
                task_id: UNKNOWN_TASK_ID,
                spawn_loc_id: UNKNOWN_SPAWN_LOCATION_ID,
            },
            TelemetryEvent::PollEnd {
                timestamp_nanos: 2_000_000,
                worker_id: 0,
            },
            TelemetryEvent::CpuSample {
                timestamp_nanos: 3_000_000, // after poll ended
                worker_id: 0,
                tid: 100,
                source: CpuSampleSource::CpuProfile,
                callchain: vec![],
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
                worker_id: 0,
                worker_local_queue_depth: 0,
                cpu_time_nanos: 0,
            },
            TelemetryEvent::WorkerUnpark {
                timestamp_nanos: 6_000_000,
                worker_id: 0,
                worker_local_queue_depth: 0,
                cpu_time_nanos: 0,
                sched_wait_delta_nanos: 0,
            },
        ];
        let idle = detect_idle_workers(&events);
        assert!(idle.is_empty()); // no idle periods flagged because global queue was empty
    }
}
