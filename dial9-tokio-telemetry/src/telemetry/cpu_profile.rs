//! CPU profiling integration: merges perf stack traces into the telemetry stream.
//!
//! When enabled, a process-wide `PerfSampler` captures CPU stack traces at a
//! configurable frequency. The flush thread drains raw samples; the caller
//! (EventWriter) maps OS thread IDs to worker IDs via SharedState.thread_roles.

use crate::telemetry::events::{CpuSampleSource, ThreadName};
use dial9_perf_self_profile::tracepoint::TracepointDef;
use dial9_perf_self_profile::{EventSource, PerfSampler, SamplerConfig};
use dial9_trace_format::types::FieldValue;
use std::collections::HashMap;
use std::io;

/// A kernel tracepoint to capture into the trace.
///
/// Tracepoints are identified by `subsystem:event` names matching the tracefs
/// layout at `/sys/kernel/debug/tracing/events/<subsystem>/<event>/format`.
///
/// # Examples
///
/// ```no_run
/// use dial9_tokio_telemetry::telemetry::cpu_profile::Tracepoint;
///
/// // Infallible — just stores the names
/// let tp = Tracepoint::new("sched", "sched_switch");
///
/// // Parse the perf-style "subsystem:event" format
/// let tp = Tracepoint::parse("sched:sched_switch").unwrap();
///
/// // Optionally validate against tracefs ahead of time
/// let tp = tp.validate().expect("tracepoint not found");
/// ```
#[derive(Debug, Clone)]
pub struct Tracepoint(TracepointInner);

#[derive(Debug, Clone)]
enum TracepointInner {
    Named { subsystem: String, event: String },
    Resolved(TracepointDef),
}

impl Tracepoint {
    /// Create a tracepoint from subsystem and event names. Infallible.
    pub fn new(subsystem: impl Into<String>, event: impl Into<String>) -> Self {
        Self(TracepointInner::Named {
            subsystem: subsystem.into(),
            event: event.into(),
        })
    }

    /// Parse a `"subsystem:event"` string (the format used by `perf record -e`).
    pub fn parse(s: &str) -> io::Result<Self> {
        let (subsystem, event) = s.split_once(':').ok_or_else(|| {
            io::Error::new(
                io::ErrorKind::InvalidInput,
                format!("expected 'subsystem:event', got '{s}'"),
            )
        })?;
        Ok(Self::new(subsystem, event))
    }

    /// Resolve the tracepoint against tracefs, returning a validated instance.
    ///
    /// Reads the format file at `/sys/kernel/debug/tracing/events/<subsystem>/<event>/format`.
    /// Fails if the tracepoint doesn't exist or tracefs is not accessible.
    pub fn validate(self) -> io::Result<Self> {
        match self.0 {
            TracepointInner::Resolved(_) => Ok(self),
            TracepointInner::Named {
                ref subsystem,
                ref event,
            } => {
                let def = TracepointDef::from_event(subsystem, event)?;
                Ok(Self(TracepointInner::Resolved(def)))
            }
        }
    }

    /// Resolve if needed and return the `TracepointDef`.
    pub(crate) fn into_def(self) -> io::Result<TracepointDef> {
        match self.0 {
            TracepointInner::Resolved(def) => Ok(def),
            TracepointInner::Named { subsystem, event } => {
                TracepointDef::from_event(&subsystem, &event)
            }
        }
    }
}

/// Read the thread name from `/proc/self/task/<tid>/comm`.
/// Returns `None` if the file can't be read.
pub(crate) fn read_thread_name(tid: u32) -> Option<String> {
    std::fs::read_to_string(format!("/proc/self/task/{tid}/comm"))
        .ok()
        .map(|s| s.trim().to_string())
        .filter(|s| !s.is_empty())
}

/// Configuration for CPU profiling integration.
#[derive(Debug, Clone)]
pub struct CpuProfilingConfig {
    /// Sampling frequency in Hz. Default: 99 (low overhead).
    pub frequency_hz: u64,
    /// Which perf event source to use.
    pub event_source: EventSource,
    /// Whether to include kernel stack frames.
    pub include_kernel: bool,
}

impl Default for CpuProfilingConfig {
    fn default() -> Self {
        Self {
            frequency_hz: 99,
            event_source: EventSource::SwCpuClock,
            include_kernel: false,
        }
    }
}

/// Configuration for per-worker sched event capture (context switches).
///
/// Uses `perf_event_open` with `SwContextSwitches` in per-thread mode,
/// so each worker thread gets its own perf fd via `on_thread_start`.
#[derive(Debug, Clone, Default)]
pub struct SchedEventConfig {
    /// Whether to include kernel stack frames.
    pub include_kernel: bool,
}

/// A raw CPU sample before worker-id resolution.
pub(crate) struct RawCpuSample {
    pub tid: u32,
    pub timestamp_nanos: u64,
    pub callchain: Vec<u64>,
    pub source: CpuSampleSource,
}

/// Manages the process-wide perf sampler. Yields raw samples without worker IDs.
pub(crate) struct CpuProfiler {
    sampler: PerfSampler,
    pid: u32,
    /// OS tid → thread name, eagerly cached at drain time so short-lived threads
    /// are captured before they exit and `/proc/self/task/<tid>/comm` disappears.
    tid_to_name: HashMap<u32, ThreadName>,
}

impl CpuProfiler {
    pub fn start(config: CpuProfilingConfig) -> io::Result<Self> {
        let sampler = PerfSampler::start(SamplerConfig {
            frequency_hz: config.frequency_hz,
            event_source: config.event_source,
            include_kernel: config.include_kernel,
        })?;
        Ok(Self {
            sampler,
            pid: std::process::id(),
            tid_to_name: HashMap::new(),
        })
    }

    /// Drain all pending perf samples as raw (tid, callchain) tuples.
    ///
    /// Filters out child-process samples (perf `inherit` leaks them).
    /// Eagerly caches thread names for non-worker tids.
    pub fn drain(&mut self, mut f: impl FnMut(RawCpuSample, Option<&ThreadName>)) {
        let pid = self.pid;
        self.sampler.for_each_sample(|sample| {
            if sample.pid != pid {
                return;
            }
            if !self.tid_to_name.contains_key(&sample.tid)
                && let Some(name) = read_thread_name(sample.tid)
            {
                self.tid_to_name.insert(sample.tid, ThreadName::new(name));
            }
            let thread_name = self.tid_to_name.get(&sample.tid);
            f(
                RawCpuSample {
                    tid: sample.tid,
                    timestamp_nanos: sample.time,
                    callchain: sample.callchain.clone(),
                    source: CpuSampleSource::CpuProfile,
                },
                thread_name,
            );
        });
    }
}

/// Per-thread sched event profiler. Yields raw samples without worker IDs.
pub(crate) struct SchedProfiler {
    sampler: PerfSampler,
}

impl SchedProfiler {
    pub fn new(config: SchedEventConfig) -> io::Result<Self> {
        let sampler = PerfSampler::new_per_thread(SamplerConfig {
            frequency_hz: 1,
            event_source: EventSource::SwContextSwitches,
            include_kernel: config.include_kernel,
        })?;
        Ok(Self { sampler })
    }

    pub fn track_current_thread(&mut self) -> io::Result<()> {
        self.sampler.track_current_thread()
    }

    pub fn stop_tracking_current_thread(&mut self) {
        self.sampler.stop_tracking_current_thread()
    }

    pub fn drain(&mut self, mut f: impl FnMut(RawCpuSample)) {
        self.sampler.for_each_sample(|sample| {
            f(RawCpuSample {
                tid: sample.tid,
                timestamp_nanos: sample.time,
                callchain: sample.callchain.clone(),
                source: CpuSampleSource::SchedEvent,
            });
        });
    }
}

/// A raw tracepoint event ready for encoding into the trace.
pub(crate) struct RawTracepointEvent {
    /// Pre-built schema (Arc-backed, cheap to clone).
    pub schema: dial9_trace_format::encoder::Schema,
    /// Timestamp in nanoseconds (CLOCK_MONOTONIC).
    pub timestamp_ns: u64,
    /// OS thread ID from the perf sample.
    pub tid: u32,
    /// Field values (no leading timestamp — that's separate).
    /// Includes [tid, cpu, ...tracepoint fields].
    pub values: Vec<dial9_trace_format::types::FieldValue>,
}

/// Captures kernel tracepoint events via `perf_event_open` and produces
/// trace-format-ready events. Uses per-thread mode — each worker thread
/// is tracked individually via `on_thread_start`/`on_thread_stop`.
pub(crate) struct TracepointProfiler {
    sampler: PerfSampler,
    def: dial9_perf_self_profile::tracepoint::TracepointDef,
    /// Wrapper schema: [tid, cpu, callchain, ...tracepoint fields].
    schema: dial9_trace_format::encoder::Schema,
}

impl TracepointProfiler {
    pub fn new(def: dial9_perf_self_profile::tracepoint::TracepointDef) -> io::Result<Self> {
        let sampler = PerfSampler::new_per_thread(SamplerConfig {
            frequency_hz: 1,
            event_source: def.event_source(),
            include_kernel: false,
        })?;
        // Build a wrapper schema that includes perf sample metadata
        // before the tracepoint's own fields.
        let mut fields = vec![
            dial9_trace_format::schema::FieldDef {
                name: "worker_id".to_string(),
                field_type: dial9_trace_format::types::FieldType::Varint,
            },
            dial9_trace_format::schema::FieldDef {
                name: "tid".to_string(),
                field_type: dial9_trace_format::types::FieldType::Varint,
            },
            dial9_trace_format::schema::FieldDef {
                name: "cpu".to_string(),
                field_type: dial9_trace_format::types::FieldType::Varint,
            },
        ];
        fields.extend(def.to_trace_format_fields());
        let schema = dial9_trace_format::encoder::Schema::new(&def.name, fields);
        Ok(Self {
            sampler,
            def,
            schema,
        })
    }

    pub fn track_current_thread(&mut self) -> io::Result<()> {
        self.sampler.track_current_thread()
    }

    pub fn stop_tracking_current_thread(&mut self) {
        self.sampler.stop_tracking_current_thread()
    }

    /// Drain pending samples, converting each to a `RawTracepointEvent`.
    ///
    /// Values contain `[tid, cpu, ...tracepoint fields]`. The caller is
    /// responsible for prepending `worker_id` (resolved from `tid`).
    pub fn drain(&mut self, mut f: impl FnMut(RawTracepointEvent)) {
        let def = &self.def;
        let schema = &self.schema;
        self.sampler.for_each_sample(|sample| {
            let Some(raw) = &sample.raw else { return };
            let extracted = match def.extract_fields(raw) {
                Ok(e) => e,
                Err(e) => {
                    tracing::warn!("tracepoint field extraction failed: {e}");
                    return;
                }
            };
            let tp_values = def.to_trace_format_values(&extracted);
            // Leave worker_id slot for the caller to fill via insert(0, ...).
            let mut values = Vec::with_capacity(3 + tp_values.len());
            values.push(FieldValue::Varint(0)); // placeholder for worker_id
            values.push(FieldValue::Varint(sample.tid as u64));
            values.push(FieldValue::Varint(sample.cpu as u64));
            values.extend(tp_values);
            f(RawTracepointEvent {
                schema: schema.clone(),
                timestamp_ns: sample.time,
                tid: sample.tid,
                values,
            });
        });
    }
}
