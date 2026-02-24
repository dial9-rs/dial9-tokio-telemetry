use crate::telemetry::task_metadata::{SpawnLocationId, TaskId};
use serde::Serialize;

/// Wire event representing a telemetry record after interning.
///
/// Compare with `RawEvent` which is emitted by worker threads and carries
/// `&'static Location` instead of interned `SpawnLocationId`.
///
/// Future updates will continue to diverge the in-memory format with the wire format.
///
/// NOTE: the `Serialize` impl here is just for convienence of writing to JSON.
/// It does NOT reflect the wire format.
#[derive(Debug, Clone, PartialEq, Serialize)]
#[serde(tag = "event")]
pub enum TelemetryEvent {
    PollStart {
        #[serde(rename = "timestamp_ns")]
        timestamp_nanos: u64,
        #[serde(rename = "worker")]
        worker_id: usize,
        #[serde(rename = "local_q")]
        worker_local_queue_depth: usize,
        task_id: TaskId,
        spawn_loc_id: SpawnLocationId,
    },
    PollEnd {
        #[serde(rename = "timestamp_ns")]
        timestamp_nanos: u64,
        #[serde(rename = "worker")]
        worker_id: usize,
    },
    WorkerPark {
        #[serde(rename = "timestamp_ns")]
        timestamp_nanos: u64,
        #[serde(rename = "worker")]
        worker_id: usize,
        #[serde(rename = "local_q")]
        worker_local_queue_depth: usize,
        /// Thread CPU time (nanos) from CLOCK_THREAD_CPUTIME_ID.
        #[serde(rename = "cpu_ns")]
        cpu_time_nanos: u64,
    },
    WorkerUnpark {
        #[serde(rename = "timestamp_ns")]
        timestamp_nanos: u64,
        #[serde(rename = "worker")]
        worker_id: usize,
        #[serde(rename = "local_q")]
        worker_local_queue_depth: usize,
        /// Thread CPU time (nanos) from CLOCK_THREAD_CPUTIME_ID.
        #[serde(rename = "cpu_ns")]
        cpu_time_nanos: u64,
        /// Scheduling wait delta (nanos) from schedstat.
        #[serde(rename = "sched_wait_ns")]
        sched_wait_delta_nanos: u64,
    },
    QueueSample {
        #[serde(rename = "timestamp_ns")]
        timestamp_nanos: u64,
        #[serde(rename = "global_q")]
        global_queue_depth: usize,
    },
    SpawnLocationDef {
        id: SpawnLocationId,
        location: String,
    },
    TaskSpawn {
        task_id: TaskId,
        spawn_loc_id: SpawnLocationId,
    },
    /// A CPU stack trace sample from perf_event, attributed to a worker thread.
    CpuSample {
        #[serde(rename = "timestamp_ns")]
        timestamp_nanos: u64,
        #[serde(rename = "worker")]
        worker_id: usize,
        /// Raw instruction pointer addresses (leaf first). Symbolized offline.
        callchain: Vec<u64>,
    },
}

impl TelemetryEvent {
    /// Returns the timestamp in nanoseconds, if this event type carries one.
    ///
    /// `SpawnLocationDef` and `TaskSpawn` are metadata records without timestamps.
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
            } => Some(*timestamp_nanos),
            TelemetryEvent::SpawnLocationDef { .. }
            | TelemetryEvent::TaskSpawn { .. } => None,
        }
    }

    /// Returns the worker ID, if this event type is associated with a worker.
    pub fn worker_id(&self) -> Option<usize> {
        match self {
            TelemetryEvent::PollStart { worker_id, .. }
            | TelemetryEvent::PollEnd { worker_id, .. }
            | TelemetryEvent::WorkerPark { worker_id, .. }
            | TelemetryEvent::WorkerUnpark { worker_id, .. }
            | TelemetryEvent::CpuSample { worker_id, .. } => Some(*worker_id),
            TelemetryEvent::QueueSample { .. }
            | TelemetryEvent::SpawnLocationDef { .. }
            | TelemetryEvent::TaskSpawn { .. } => None,
        }
    }

    /// Returns true if this is a runtime event (has a timestamp), as opposed to
    /// a metadata record (SpawnLocationDef, TaskSpawn).
    pub fn is_runtime_event(&self) -> bool {
        self.timestamp_nanos().is_some()
    }
}

/// Raw event emitted by worker threads into thread-local buffers.
/// Carries rich data (including `&'static Location`) with no locking.
/// Converted to wire format by the flush thread.
#[derive(Debug, Clone, Copy)]
pub enum RawEvent {
    PollStart {
        timestamp_nanos: u64,
        worker_id: usize,
        worker_local_queue_depth: usize,
        task_id: crate::telemetry::task_metadata::TaskId,
        location: &'static std::panic::Location<'static>,
    },
    PollEnd {
        timestamp_nanos: u64,
        worker_id: usize,
    },
    WorkerPark {
        timestamp_nanos: u64,
        worker_id: usize,
        worker_local_queue_depth: usize,
        cpu_time_nanos: u64,
    },
    WorkerUnpark {
        timestamp_nanos: u64,
        worker_id: usize,
        worker_local_queue_depth: usize,
        cpu_time_nanos: u64,
        sched_wait_delta_nanos: u64,
    },
    QueueSample {
        timestamp_nanos: u64,
        global_queue_depth: usize,
    },
    TaskSpawn {
        task_id: crate::telemetry::task_metadata::TaskId,
        location: &'static std::panic::Location<'static>,
    },
    /// Reports this worker thread's OS tid to the flush thread (internal only, not serialized).
    WorkerTid {
        worker_id: usize,
        tid: u32,
    },
}

/// Get the OS thread ID (tid) of the calling thread via `gettid()`.
pub fn current_tid() -> u32 {
    unsafe { libc::syscall(libc::SYS_gettid) as u32 }
}

/// Read the calling thread's CPU time via `CLOCK_THREAD_CPUTIME_ID`.
/// This is a vDSO call on Linux (~20-40ns), no actual syscall.
pub fn thread_cpu_time_nanos() -> u64 {
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

/// Per-thread scheduler stats from /proc/<pid>/task/<tid>/schedstat.
/// Fields: run_time_ns wait_time_ns timeslices
#[derive(Debug, Clone, Copy, Default)]
pub struct SchedStat {
    pub wait_time_ns: u64,
}

impl SchedStat {
    /// Read schedstat for the current thread using a cached per-thread file descriptor.
    /// Opening /proc/self/task/<tid>/schedstat is done once per thread; subsequent reads
    /// use `pread(fd, buf, 0)` which is ~2-3x cheaper than open+read+close.
    pub fn read_current() -> std::io::Result<Self> {
        use std::os::unix::io::RawFd;

        thread_local! {
            // -1 means not yet opened
            static SCHED_FD: std::cell::Cell<RawFd> = const { std::cell::Cell::new(-1) };
        }

        let fd = SCHED_FD.with(|cell| {
            let fd = cell.get();
            if fd >= 0 {
                return fd;
            }
            // First call on this thread: open the file
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
            if new_fd >= 0 {
                cell.set(new_fd);
            }
            new_fd
        });

        if fd < 0 {
            return Err(std::io::Error::last_os_error());
        }

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
        Self::parse(s)
            .ok_or_else(|| std::io::Error::new(std::io::ErrorKind::InvalidData, "bad schedstat"))
    }

    fn parse(s: &str) -> Option<Self> {
        let mut parts = s.split_whitespace();
        let _run_time_ns: u64 = parts.next()?.parse().ok()?;
        let wait_time_ns: u64 = parts.next()?.parse().ok()?;
        Some(SchedStat { wait_time_ns })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::telemetry::task_metadata::{UNKNOWN_SPAWN_LOCATION_ID, UNKNOWN_TASK_ID};

    #[test]
    fn test_telemetry_event_timestamp() {
        let poll_start = TelemetryEvent::PollStart {
            timestamp_nanos: 1000,
            worker_id: 0,
            worker_local_queue_depth: 2,
            task_id: UNKNOWN_TASK_ID,
            spawn_loc_id: UNKNOWN_SPAWN_LOCATION_ID,
        };
        assert_eq!(poll_start.timestamp_nanos(), Some(1000));

        let poll_end = TelemetryEvent::PollEnd {
            timestamp_nanos: 2000,
            worker_id: 1,
        };
        assert_eq!(poll_end.timestamp_nanos(), Some(2000));

        let queue_sample = TelemetryEvent::QueueSample {
            timestamp_nanos: 3000,
            global_queue_depth: 5,
        };
        assert_eq!(queue_sample.timestamp_nanos(), Some(3000));

        let spawn_def = TelemetryEvent::SpawnLocationDef {
            id: SpawnLocationId::from_u16(1),
            location: "src/main.rs:10:5".to_string(),
        };
        assert_eq!(spawn_def.timestamp_nanos(), None);

        let task_spawn = TelemetryEvent::TaskSpawn {
            task_id: TaskId::from_u32(1),
            spawn_loc_id: SpawnLocationId::from_u16(1),
        };
        assert_eq!(task_spawn.timestamp_nanos(), None);
    }

    #[test]
    fn test_telemetry_event_worker_id() {
        let poll_start = TelemetryEvent::PollStart {
            timestamp_nanos: 1000,
            worker_id: 3,
            worker_local_queue_depth: 0,
            task_id: UNKNOWN_TASK_ID,
            spawn_loc_id: UNKNOWN_SPAWN_LOCATION_ID,
        };
        assert_eq!(poll_start.worker_id(), Some(3));

        let queue_sample = TelemetryEvent::QueueSample {
            timestamp_nanos: 1000,
            global_queue_depth: 5,
        };
        assert_eq!(queue_sample.worker_id(), None);

        let spawn_def = TelemetryEvent::SpawnLocationDef {
            id: SpawnLocationId::from_u16(1),
            location: "test".to_string(),
        };
        assert_eq!(spawn_def.worker_id(), None);
    }

    #[test]
    fn test_is_runtime_event() {
        let poll_start = TelemetryEvent::PollStart {
            timestamp_nanos: 1000,
            worker_id: 0,
            worker_local_queue_depth: 0,
            task_id: UNKNOWN_TASK_ID,
            spawn_loc_id: UNKNOWN_SPAWN_LOCATION_ID,
        };
        assert!(poll_start.is_runtime_event());

        let spawn_def = TelemetryEvent::SpawnLocationDef {
            id: SpawnLocationId::from_u16(1),
            location: "test".to_string(),
        };
        assert!(!spawn_def.is_runtime_event());

        let task_spawn = TelemetryEvent::TaskSpawn {
            task_id: TaskId::from_u32(1),
            spawn_loc_id: SpawnLocationId::from_u16(1),
        };
        assert!(!task_spawn.is_runtime_event());
    }

    #[test]
    fn test_telemetry_event_creation() {
        let event = TelemetryEvent::PollStart {
            timestamp_nanos: 1000,
            worker_id: 0,
            worker_local_queue_depth: 2,
            task_id: UNKNOWN_TASK_ID,
            spawn_loc_id: UNKNOWN_SPAWN_LOCATION_ID,
        };
        assert_eq!(event.timestamp_nanos(), Some(1000));
        assert_eq!(event.worker_id(), Some(0));
    }

    #[test]
    fn test_telemetry_event_clone() {
        let event = TelemetryEvent::SpawnLocationDef {
            id: SpawnLocationId::from_u16(42),
            location: "src/lib.rs:1:1".to_string(),
        };
        let cloned = event.clone();
        assert_eq!(event, cloned);
    }
}
