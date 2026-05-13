use crate::telemetry::{format::WorkerId, task_metadata::TaskId};
use dial9_trace_format::{FieldValue, InternedString};
use serde::Serialize;
#[cfg(feature = "cpu-profiling")]
use std::sync::Arc;

/// Role of a thread known to the telemetry system.
#[cfg(feature = "cpu-profiling")]
#[derive(Debug, Clone, Copy)]
pub(crate) enum ThreadRole {
    /// A tokio worker thread with the given index.
    Worker(usize),
    /// A thread in tokio's blocking pool.
    Blocking,
}

/// What triggered a [`TelemetryEvent::CpuSample`].
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
pub enum CpuSampleSource {
    /// Periodic CPU profiling sample (frequency-based).
    CpuProfile = 0,
    /// Context switch captured by per-thread sched event tracking.
    SchedEvent = 1,
}

impl CpuSampleSource {
    /// Decode from a raw `u8` wire value.
    pub fn from_u8(v: u8) -> Self {
        match v {
            1 => Self::SchedEvent,
            _ => Self::CpuProfile,
        }
    }
}

#[cfg(feature = "cpu-profiling")]
#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) struct ThreadName(Arc<str>);

#[cfg(feature = "cpu-profiling")]
impl ThreadName {
    pub(crate) fn new(name: String) -> Self {
        Self(name.into())
    }

    pub(crate) fn as_str(&self) -> &str {
        self.0.as_ref()
    }
}

/// Wire event representing a telemetry record after interning.
///
/// This is the decoded representation produced by reading a trace back — the
/// on-disk wire format goes through the `TraceEvent` structs defined in
/// [`crate::telemetry::format`]. Worker threads encode those `TraceEvent`s
/// directly; this enum is only constructed during decode for analysis.
///
/// NOTE: the `Serialize` impl here is just for convienence of writing to JSON.
/// It does NOT reflect the wire format.
#[derive(Debug, Clone, PartialEq, Serialize)]
#[serde(tag = "event")]
pub enum TelemetryEvent {
    /// A task poll began on a worker thread.
    PollStart {
        /// Wall-clock timestamp in nanoseconds (monotonic).
        #[serde(rename = "timestamp_ns")]
        timestamp_nanos: u64,
        /// Worker thread that is polling.
        #[serde(rename = "worker")]
        worker_id: WorkerId,
        /// Depth of this worker's local task queue at poll start.
        #[serde(rename = "local_q")]
        worker_local_queue_depth: usize,
        /// Task being polled.
        task_id: TaskId,
        /// Interned spawn location of the task.
        spawn_loc: InternedString,
    },
    /// A task poll completed on a worker thread.
    PollEnd {
        /// Wall-clock timestamp in nanoseconds (monotonic).
        #[serde(rename = "timestamp_ns")]
        timestamp_nanos: u64,
        /// Worker thread that finished polling.
        #[serde(rename = "worker")]
        worker_id: WorkerId,
    },
    /// A worker thread parked (went idle).
    WorkerPark {
        /// Wall-clock timestamp in nanoseconds (monotonic).
        #[serde(rename = "timestamp_ns")]
        timestamp_nanos: u64,
        /// Worker thread that parked.
        #[serde(rename = "worker")]
        worker_id: WorkerId,
        /// Depth of this worker's local task queue at park time.
        #[serde(rename = "local_q")]
        worker_local_queue_depth: usize,
        /// Thread CPU time (nanos) from CLOCK_THREAD_CPUTIME_ID.
        #[serde(rename = "cpu_ns")]
        cpu_time_nanos: u64,
    },
    /// A worker thread unparked (resumed).
    WorkerUnpark {
        /// Wall-clock timestamp in nanoseconds (monotonic).
        #[serde(rename = "timestamp_ns")]
        timestamp_nanos: u64,
        /// Worker thread that unparked.
        #[serde(rename = "worker")]
        worker_id: WorkerId,
        /// Depth of this worker's local task queue at unpark time.
        #[serde(rename = "local_q")]
        worker_local_queue_depth: usize,
        /// Thread CPU time (nanos) from CLOCK_THREAD_CPUTIME_ID.
        #[serde(rename = "cpu_ns")]
        cpu_time_nanos: u64,
        /// Scheduling wait delta (nanos) from schedstat.
        #[serde(rename = "sched_wait_ns")]
        sched_wait_delta_nanos: u64,
    },
    /// Periodic sample of the global task queue depth.
    QueueSample {
        /// Wall-clock timestamp in nanoseconds (monotonic).
        #[serde(rename = "timestamp_ns")]
        timestamp_nanos: u64,
        /// Number of tasks in the global (injection) queue.
        #[serde(rename = "global_q")]
        global_queue_depth: usize,
    },
    /// A new task was spawned.
    TaskSpawn {
        /// Wall-clock timestamp in nanoseconds (monotonic).
        #[serde(rename = "timestamp_ns")]
        timestamp_nanos: u64,
        /// Unique identifier for the spawned task.
        task_id: TaskId,
        /// Interned spawn location of the task.
        spawn_loc: InternedString,
        /// Whether this task was spawned via
        /// [`TelemetryHandle::spawn`](crate::telemetry::TelemetryHandle::spawn).
        /// `None` for traces recorded before this field existed.
        instrumented: Option<bool>,
    },
    /// A task terminated (completed or was cancelled).
    TaskTerminate {
        /// Wall-clock timestamp in nanoseconds (monotonic).
        #[serde(rename = "timestamp_ns")]
        timestamp_nanos: u64,
        /// Task that terminated.
        task_id: TaskId,
    },
    /// A CPU stack trace sample from perf_event, attributed to a worker thread.
    CpuSample {
        /// Wall-clock timestamp in nanoseconds (monotonic).
        #[serde(rename = "timestamp_ns")]
        timestamp_nanos: u64,
        /// Worker thread that was sampled.
        #[serde(rename = "worker")]
        worker_id: WorkerId,
        /// OS thread ID that was sampled.
        tid: u32,
        /// Thread name from `/proc/self/task/<tid>/comm`, if known.
        thread_name: Option<String>,
        /// What triggered this sample.
        source: CpuSampleSource,
        /// Raw instruction pointer addresses (leaf first). Symbolized offline.
        callchain: Vec<u64>,
        /// CPU the sample was taken on, if the backend could determine it.
        /// Perf sampling fills this in; ctimer may report `None` if `getcpu`
        /// fails. Older traces recorded before this field existed decode as `None`.
        cpu: Option<u32>,
    },
    /// Maps an OS thread ID to its name (from `/proc/self/task/<tid>/comm`).
    /// Emitted before the first CpuSample referencing this tid in each file.
    /// Allows grouping non-worker CPU samples by thread name.
    ThreadNameDef {
        /// OS thread ID.
        tid: u32,
        /// Human-readable thread name.
        name: String,
    },
    /// Async backtrace captured at a yield point after the task was idle
    /// longer than the configured threshold. Instruction pointers are
    /// symbolized offline.
    TaskDump {
        /// Wall-clock timestamp in nanoseconds (monotonic) — capture time.
        #[serde(rename = "timestamp_ns")]
        timestamp_nanos: u64,
        /// Task that was idle.
        task_id: TaskId,
        /// Raw instruction pointer addresses (leaf first).
        callchain: Vec<u64>,
    },
    /// One task woke another task.
    WakeEvent {
        /// Wall-clock timestamp in nanoseconds (monotonic).
        #[serde(rename = "timestamp_ns")]
        timestamp_nanos: u64,
        /// Task that issued the wake.
        waker_task_id: TaskId,
        /// Task that was woken.
        woken_task_id: TaskId,
        /// Worker thread index that issued the wake (255 = unknown).
        target_worker: u8,
    },
    /// Key-value metadata written at the start of each segment.
    /// Makes trace files self-describing (host, region, service, boot_id, etc.).
    SegmentMetadata {
        /// Wall-clock timestamp in nanoseconds (monotonic).
        timestamp_nanos: u64,
        /// Key-value metadata pairs.
        entries: Vec<(String, String)>,
    },
    /// Clock-correlation anchor pairing a monotonic timestamp with the
    /// wall-clock value captured at the same instant.
    ClockSync {
        /// Monotonic nanoseconds.
        #[serde(rename = "timestamp_ns")]
        timestamp_nanos: u64,
        /// Nanoseconds since the Unix epoch.
        #[serde(rename = "realtime_ns")]
        realtime_nanos: u64,
    },
    /// An application-defined custom event not recognized as a built-in type.
    /// Fields are stored as `FieldValue`s in schema order. Interned string
    /// fields are resolved to `FieldValue::String` at parse time.
    Custom {
        /// Monotonic nanoseconds, if the event schema has a timestamp.
        #[serde(rename = "timestamp_ns")]
        timestamp_nanos: Option<u64>,
        /// Event type name from the schema (e.g. `"RequestCompleted"`).
        name: String,
        /// Named field values in schema order.
        fields: Vec<(String, FieldValue)>,
    },
}

impl TelemetryEvent {
    /// Returns the timestamp in nanoseconds, if this event type carries one.
    pub fn timestamp_nanos(&self) -> Option<u64> {
        match self {
            TelemetryEvent::PollStart {
                timestamp_nanos, ..
            }
            | TelemetryEvent::PollEnd {
                timestamp_nanos, ..
            }
            | TelemetryEvent::WorkerPark {
                timestamp_nanos, ..
            }
            | TelemetryEvent::WorkerUnpark {
                timestamp_nanos, ..
            }
            | TelemetryEvent::QueueSample {
                timestamp_nanos, ..
            }
            | TelemetryEvent::CpuSample {
                timestamp_nanos, ..
            }
            | TelemetryEvent::TaskDump {
                timestamp_nanos, ..
            }
            | TelemetryEvent::WakeEvent {
                timestamp_nanos, ..
            }
            | TelemetryEvent::TaskSpawn {
                timestamp_nanos, ..
            }
            | TelemetryEvent::TaskTerminate {
                timestamp_nanos, ..
            } => Some(*timestamp_nanos),
            TelemetryEvent::ThreadNameDef { .. } => None,
            TelemetryEvent::SegmentMetadata {
                timestamp_nanos, ..
            }
            | TelemetryEvent::ClockSync {
                timestamp_nanos, ..
            } => Some(*timestamp_nanos),
            TelemetryEvent::Custom {
                timestamp_nanos, ..
            } => *timestamp_nanos,
        }
    }

    /// Returns the worker ID, if this event type is associated with a worker.
    pub fn worker_id(&self) -> Option<WorkerId> {
        match self {
            TelemetryEvent::PollStart { worker_id, .. }
            | TelemetryEvent::PollEnd { worker_id, .. }
            | TelemetryEvent::WorkerPark { worker_id, .. }
            | TelemetryEvent::WorkerUnpark { worker_id, .. }
            | TelemetryEvent::CpuSample { worker_id, .. } => Some(*worker_id),
            TelemetryEvent::QueueSample { .. }
            | TelemetryEvent::TaskSpawn { .. }
            | TelemetryEvent::TaskTerminate { .. }
            | TelemetryEvent::TaskDump { .. }
            | TelemetryEvent::ThreadNameDef { .. }
            | TelemetryEvent::WakeEvent { .. }
            | TelemetryEvent::SegmentMetadata { .. }
            | TelemetryEvent::ClockSync { .. }
            | TelemetryEvent::Custom { .. } => None,
        }
    }

    /// Returns true if this is a runtime event (has a timestamp), as opposed to
    /// a metadata record.
    pub fn is_runtime_event(&self) -> bool {
        self.timestamp_nanos().is_some()
    }
}

/// Data for a CPU stack trace sample. Implements `Encodable` so samples can
/// be written directly into the thread-local trace buffer without an
/// intermediate enum. Interning of `thread_name` and `callchain` happens in
/// the `Encodable::encode` impl.
#[cfg(feature = "cpu-profiling")]
#[derive(Debug, Clone)]
pub(crate) struct CpuSampleData {
    pub timestamp_nanos: u64,
    pub worker_id: WorkerId,
    pub tid: u32,
    pub thread_name: Option<ThreadName>,
    pub source: CpuSampleSource,
    pub callchain: Vec<u64>,
    /// CPU the sample was taken on, if the backend could determine it.
    pub cpu: Option<u32>,
}

/// Get the OS thread ID (tid) of the calling thread via `gettid()`.
#[cfg(any(target_os = "linux", target_os = "android"))]
pub(crate) fn current_tid() -> u32 {
    unsafe { libc::syscall(libc::SYS_gettid) as u32 }
}

#[cfg(not(any(target_os = "linux", target_os = "android")))]
pub(crate) fn current_tid() -> u32 {
    // No gettid on non-Linux; use a thread-local counter as a unique ID.
    use std::sync::atomic::{AtomicU32, Ordering};
    static NEXT: AtomicU32 = AtomicU32::new(1);
    thread_local! { static TID: u32 = NEXT.fetch_add(1, Ordering::Relaxed); }
    TID.with(|t| *t)
}

/// Read the calling thread's CPU time via `CLOCK_THREAD_CPUTIME_ID`.
/// This is a vDSO call on Linux (~20-40ns), no actual syscall.
#[cfg(target_os = "linux")]
pub(crate) fn thread_cpu_time_nanos() -> u64 {
    let mut ts = libc::timespec {
        tv_sec: 0,
        tv_nsec: 0,
    };
    // SAFETY: `ts` is a valid, initialized timespec on the stack.
    // CLOCK_THREAD_CPUTIME_ID is always available on Linux and always succeeds.
    unsafe {
        libc::clock_gettime(libc::CLOCK_THREAD_CPUTIME_ID, &mut ts);
    }
    ts.tv_sec as u64 * 1_000_000_000 + ts.tv_nsec as u64
}

#[cfg(not(target_os = "linux"))]
pub(crate) fn thread_cpu_time_nanos() -> u64 {
    0
}

/// Read `CLOCK_MONOTONIC` in nanoseconds. Used as the single time base for
/// all trace timestamps (poll events, CPU samples, sched events).
#[cfg(target_os = "linux")]
pub fn clock_monotonic_ns() -> u64 {
    let mut ts = libc::timespec {
        tv_sec: 0,
        tv_nsec: 0,
    };
    unsafe {
        libc::clock_gettime(libc::CLOCK_MONOTONIC, &mut ts);
    }
    ts.tv_sec as u64 * 1_000_000_000 + ts.tv_nsec as u64
}

/// `CLOCK_MONOTONIC` in nanoseconds. Non-Linux fallback: elapsed time
/// since the first call on this process via `Instant`.
#[cfg(not(target_os = "linux"))]
pub fn clock_monotonic_ns() -> u64 {
    use std::sync::OnceLock;
    use std::time::Instant;
    static EPOCH: OnceLock<Instant> = OnceLock::new();
    EPOCH.get_or_init(Instant::now).elapsed().as_nanos() as u64
}

/// `CLOCK_REALTIME` in nanoseconds since the Unix epoch.
#[cfg(target_os = "linux")]
pub(crate) fn clock_realtime_ns() -> u64 {
    let mut ts = libc::timespec {
        tv_sec: 0,
        tv_nsec: 0,
    };
    unsafe {
        libc::clock_gettime(libc::CLOCK_REALTIME, &mut ts);
    }
    ts.tv_sec as u64 * 1_000_000_000 + ts.tv_nsec as u64
}

#[cfg(not(target_os = "linux"))]
pub(crate) fn clock_realtime_ns() -> u64 {
    use std::time::{SystemTime, UNIX_EPOCH};
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .expect("system clock should not be before the unix epoch")
        .as_nanos() as u64
}

/// Snapshot `(monotonic_ns, realtime_ns)` as close together as possible.
/// Reads M₁ -> R -> M₂ and pairs `R` with the midpoint of M₁ and M₂ so
/// the correlation error is half the `clock_gettime` interval.
pub(crate) fn clock_pair() -> (u64, u64) {
    let m1 = clock_monotonic_ns();
    let r = clock_realtime_ns();
    let m2 = clock_monotonic_ns();
    let mono = m1 + m2.saturating_sub(m1) / 2;
    (mono, r)
}

/// Per-thread scheduler stats from `/proc/<pid>/task/<tid>/schedstat`.
/// Fields: run_time_ns wait_time_ns timeslices
#[derive(Debug, Clone, Copy)]
pub(crate) struct SchedStat {
    pub wait_time_ns: u64,
    /// Raw fd backing this read, exposed for FD-lifecycle tests. Not used in production.
    #[cfg(all(test, any(target_os = "linux", target_os = "android")))]
    fd: std::os::fd::RawFd,
}

#[cfg(any(target_os = "linux", target_os = "android"))]
impl SchedStat {
    /// Read schedstat for the current thread using a cached per-thread file descriptor.
    /// Opening `/proc/self/task/<tid>/schedstat` is done once per thread; subsequent reads
    /// use `pread(fd, buf, 0)` which is ~2-3x cheaper than open+read+close.
    pub(crate) fn read_current() -> std::io::Result<Self> {
        use std::os::fd::{AsRawFd, FromRawFd, OwnedFd, RawFd};

        thread_local! {
            static SCHED_FD: std::cell::RefCell<Option<OwnedFd>> = const { std::cell::RefCell::new(None) };
        }

        let fd = SCHED_FD.with(|cell| -> std::io::Result<RawFd> {
            if let Some(fd) = cell.borrow().as_ref() {
                return Ok(fd.as_raw_fd());
            }
            // First call on this thread: open the file.
            // SAFETY: SYS_gettid takes no arguments and always succeeds; unsafe is
            // required because syscall() is a raw FFI function with no type checking.
            let tid = unsafe { libc::syscall(libc::SYS_gettid) } as u32;
            let path = format!("/proc/self/task/{tid}/schedstat\0");
            // SAFETY: `path` is a valid NUL-terminated string. O_RDONLY|O_CLOEXEC
            // are valid flags. The returned fd (or -1 on error) is checked below.
            let new_fd = unsafe {
                libc::open(
                    path.as_ptr() as *const libc::c_char,
                    libc::O_RDONLY | libc::O_CLOEXEC,
                )
            };
            if new_fd < 0 {
                return Err(std::io::Error::last_os_error());
            }
            // SAFETY: new_fd was just returned by open() and is owned by us. OwnedFd
            // takes ownership and will close it on drop (including on thread exit).
            let owned = unsafe { OwnedFd::from_raw_fd(new_fd) };
            let raw = owned.as_raw_fd();
            *cell.borrow_mut() = Some(owned);
            Ok(raw)
        })?;

        let mut buf = [0u8; 64];
        // SAFETY: `fd` is a valid open file descriptor (checked above). `buf` is a
        // live stack buffer of exactly `buf.len()` bytes. pread does not advance the
        // file offset, so concurrent calls on the same fd from other threads are safe.
        let n = unsafe { libc::pread(fd, buf.as_mut_ptr() as *mut libc::c_void, buf.len(), 0) };
        if n <= 0 {
            return Err(std::io::Error::last_os_error());
        }
        let s = std::str::from_utf8(&buf[..n as usize]).map_err(|_| {
            std::io::Error::new(std::io::ErrorKind::InvalidData, "bad schedstat utf8")
        })?;
        let wait_time_ns = Self::parse_wait_time_ns(s)
            .ok_or_else(|| std::io::Error::new(std::io::ErrorKind::InvalidData, "bad schedstat"))?;
        Ok(Self {
            wait_time_ns,
            #[cfg(all(test, target_os = "linux"))]
            fd,
        })
    }

    fn parse_wait_time_ns(s: &str) -> Option<u64> {
        let mut parts = s.split_whitespace();
        let _run_time_ns: u64 = parts.next()?.parse().ok()?;
        parts.next()?.parse().ok()
    }
}

#[cfg(not(any(target_os = "linux", target_os = "android")))]
impl SchedStat {
    pub(crate) fn read_current() -> std::io::Result<Self> {
        Err(std::io::Error::new(
            std::io::ErrorKind::Unsupported,
            "schedstat not available on this platform",
        ))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::telemetry::task_metadata::UNKNOWN_TASK_ID;

    const UNKNOWN_SPAWN_LOC: InternedString = InternedString::from_raw(0);

    #[test]
    fn test_telemetry_event_timestamp() {
        let poll_start = TelemetryEvent::PollStart {
            timestamp_nanos: 1000,
            worker_id: WorkerId(0),
            worker_local_queue_depth: 2,
            task_id: UNKNOWN_TASK_ID,
            spawn_loc: UNKNOWN_SPAWN_LOC,
        };
        assert_eq!(poll_start.timestamp_nanos(), Some(1000));

        let poll_end = TelemetryEvent::PollEnd {
            timestamp_nanos: 2000,
            worker_id: WorkerId(1),
        };
        assert_eq!(poll_end.timestamp_nanos(), Some(2000));

        let queue_sample = TelemetryEvent::QueueSample {
            timestamp_nanos: 3000,
            global_queue_depth: 5,
        };
        assert_eq!(queue_sample.timestamp_nanos(), Some(3000));

        let task_spawn = TelemetryEvent::TaskSpawn {
            timestamp_nanos: 5_000_000,
            task_id: TaskId::from_u32(1),
            spawn_loc: InternedString::from_raw(1),
            instrumented: Some(true),
        };
        assert_eq!(task_spawn.timestamp_nanos(), Some(5_000_000));
    }

    #[test]
    fn test_telemetry_event_worker_id() {
        let poll_start = TelemetryEvent::PollStart {
            timestamp_nanos: 1000,
            worker_id: WorkerId(3),
            worker_local_queue_depth: 0,
            task_id: UNKNOWN_TASK_ID,
            spawn_loc: UNKNOWN_SPAWN_LOC,
        };
        assert_eq!(poll_start.worker_id(), Some(WorkerId(3)));

        let queue_sample = TelemetryEvent::QueueSample {
            timestamp_nanos: 1000,
            global_queue_depth: 5,
        };
        assert_eq!(queue_sample.worker_id(), None);
    }

    #[test]
    fn test_is_runtime_event() {
        let poll_start = TelemetryEvent::PollStart {
            timestamp_nanos: 1000,
            worker_id: WorkerId(0),
            worker_local_queue_depth: 0,
            task_id: UNKNOWN_TASK_ID,
            spawn_loc: UNKNOWN_SPAWN_LOC,
        };
        assert!(poll_start.is_runtime_event());

        let task_spawn = TelemetryEvent::TaskSpawn {
            timestamp_nanos: 5_000_000,
            task_id: TaskId::from_u32(1),
            spawn_loc: InternedString::from_raw(1),
            instrumented: Some(true),
        };
        assert!(task_spawn.is_runtime_event());
    }

    #[test]
    fn test_telemetry_event_creation() {
        let event = TelemetryEvent::PollStart {
            timestamp_nanos: 1000,
            worker_id: WorkerId(0),
            worker_local_queue_depth: 2,
            task_id: UNKNOWN_TASK_ID,
            spawn_loc: UNKNOWN_SPAWN_LOC,
        };
        assert_eq!(event.timestamp_nanos(), Some(1000));
        assert_eq!(event.worker_id(), Some(WorkerId(0)));
    }

    #[test]
    fn test_telemetry_event_clone() {
        let event = TelemetryEvent::ThreadNameDef {
            tid: 42,
            name: "worker-0".to_string(),
        };
        let cloned = event.clone();
        assert_eq!(event, cloned);
    }

    #[test]
    #[cfg(any(target_os = "linux", target_os = "android"))]
    fn test_schedstat_fd_closed_on_thread_exit() {
        // We cannot just check `fcntl(fd, F_GETFD) != -1` after the thread
        // exits: the kernel readily recycles fd numbers, and other tests in the
        // suite open many files (including their own per-thread schedstat fds)
        // concurrently. A closed fd may immediately be reused by an unrelated
        // open in another thread, making a naive open-check flaky.
        //
        // Instead, record the path the fd points at (via /proc/self/fd/<fd>)
        // *inside* the spawned thread, and after join check whether the fd is
        // either closed or now points at a different file. Each thread opens
        // schedstat for its own tid, so the captured path
        // `/proc/self/task/<spawned_tid>/schedstat` is unique to the spawned
        // thread's open: no other thread will ever open that exact path. If the
        // fd is still open and still points at it, the OwnedFd was leaked.

        fn fd_target(fd: std::os::fd::RawFd) -> Option<std::path::PathBuf> {
            std::fs::read_link(format!("/proc/self/fd/{fd}")).ok()
        }

        let (fd, opened_path) = std::thread::spawn(|| {
            let fd = SchedStat::read_current().unwrap().fd;
            let path = fd_target(fd).expect("readlink /proc/self/fd/<fd> in live thread");
            (fd, path)
        })
        .join()
        .unwrap();

        // Sanity-check: confirm we actually captured a schedstat path so the
        // assertion below is meaningful (rather than passing because we read
        // the wrong link).
        assert!(
            opened_path.to_string_lossy().ends_with("/schedstat"),
            "expected schedstat path, got {opened_path:?}"
        );

        match fd_target(fd) {
            None => { /* fd is closed - good. */ }
            Some(now) if now != opened_path => {
                // fd was closed and the slot was reused for an unrelated open
                // in another thread. That still means our OwnedFd was dropped.
            }
            Some(now) => {
                panic!("schedstat fd {fd} leaked after thread exit (still points at {now:?})")
            }
        }
    }
}
