#[repr(u8)]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum EventType {
    PollStart = 0,
    PollEnd = 1,
    WorkerPark = 2,
    WorkerUnpark = 3,
    QueueSample = 4,
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

#[derive(Debug, Clone, Copy)]
pub struct MetricsSnapshot {
    pub timestamp_nanos: u64,
    pub worker_id: usize,
    pub global_queue_depth: usize,
    pub worker_local_queue_depth: usize,
    /// Thread CPU time (nanos) from CLOCK_THREAD_CPUTIME_ID. Only set on park/unpark events.
    pub cpu_time_nanos: u64,
    /// Scheduling wait delta (nanos) from schedstat. Only set on WorkerUnpark.
    pub sched_wait_delta_nanos: u64,
}

#[derive(Debug, Clone, Copy)]
pub struct TelemetryEvent {
    pub event_type: EventType,
    pub metrics: MetricsSnapshot,
}

impl TelemetryEvent {
    pub fn new(event_type: EventType, metrics: MetricsSnapshot) -> Self {
        Self {
            event_type,
            metrics,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_event_type_repr() {
        assert_eq!(EventType::PollStart as u8, 0);
        assert_eq!(EventType::PollEnd as u8, 1);
        assert_eq!(EventType::WorkerPark as u8, 2);
        assert_eq!(EventType::WorkerUnpark as u8, 3);
    }

    #[test]
    fn test_telemetry_event_creation() {
        let metrics = MetricsSnapshot {
            timestamp_nanos: 1000,
            worker_id: 0,
            global_queue_depth: 5,
            worker_local_queue_depth: 2,
            cpu_time_nanos: 0,
            sched_wait_delta_nanos: 0,
        };
        let event = TelemetryEvent::new(EventType::PollStart, metrics);
        assert_eq!(event.event_type, EventType::PollStart);
        assert_eq!(event.metrics.timestamp_nanos, 1000);
    }
}
