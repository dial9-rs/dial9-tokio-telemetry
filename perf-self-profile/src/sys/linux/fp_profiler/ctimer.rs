//! Per-thread CPU timer engine, equivalent to async-profiler's `-e ctimer`.
//!
//! Uses `timer_create(CLOCK_THREAD_CPUTIME_ID, ...)` with `SIGEV_THREAD_ID`
//! so each thread gets its own timer that fires SIGPROF *on that thread*
//! when it has consumed N nanoseconds of CPU time.
//!
//! This avoids two itimer biases:
//!   1. process-wide SIGPROF delivery picks threads without CPU-time weighting,
//!      so hot threads are undersampled.
//!   2. Only one itimer signal can be pending per process at a time, so on
//!      multi-core workloads you systematically lose samples.
//!
//! ctimer avoids both by binding each timer to a specific tid (`SIGEV_THREAD_ID`)
//! and charging against per-thread CPU time.
//!
//! # Lifecycle
//!
//! 1. Call `start(interval_ns)` once in the main thread to install the SIGPROF handler.
//! 2. Each thread calls `register_thread` from its own context to arm a per-thread
//!    timer, and `unregister_thread` (same thread) to tear it down.
//! 3. `disable` and `enable` pause sampling without disarming the timers, so resuming
//!    doesn't require re-registration.
//! 4. Teardown: `disarm_all_timers` disarms timers globally (called automatically on
//!    `CtimerSampler` drop). A subsequent `start` is required to sample again.

use std::cell::Cell;
use std::io;
use std::mem;
use std::ptr;
use std::sync::atomic::{AtomicBool, AtomicI64, Ordering};

use crate::sys::linux::gettid;

static INTERVAL_NS: AtomicI64 = AtomicI64::new(0);
/// Whether sampling is currently enabled. Toggled by `disable`/`enable`.
static RUNNING: AtomicBool = AtomicBool::new(false);
/// One-way flag to tell the signal handler to self-disarm each thread's timer.
static DISARM_REQUESTED: AtomicBool = AtomicBool::new(false);

thread_local! {
    static THREAD_TIMER: Cell<Option<libc::timer_t>> = const { Cell::new(None) };
}

/// Install SIGPROF handler and remember the sampling interval.
///
/// Must be called exactly once, from a single thread, before any `register_thread` calls.
/// Calling again replaces the handler and resets the interval without re-arming existing timers.
///
/// # Safety
///
/// `handler` runs in SIGPROF signal context and must be async-signal-safe
/// (no heap allocation, no locks, no panic).
pub unsafe fn start(
    interval_ns: i64,
    handler: extern "C" fn(libc::c_int, *mut libc::siginfo_t, *mut libc::c_void),
) -> Result<(), io::Error> {
    if interval_ns <= 0 {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "interval must be positive",
        ));
    }

    // SAFETY: zero-filled `libc::sigaction` is valid before we assign handler fields.
    let mut sa: libc::sigaction = unsafe { mem::zeroed() };
    sa.sa_sigaction = handler as usize;
    sa.sa_flags = libc::SA_SIGINFO | libc::SA_RESTART;
    // SAFETY: `sa.sa_mask` points to a valid sigset_t inside `sa`.
    unsafe { libc::sigemptyset(&mut sa.sa_mask) };

    // SAFETY: `sa` is a valid `sigaction`, `oldact` is null (allowed).
    if unsafe { libc::sigaction(libc::SIGPROF, &sa, ptr::null_mut()) } != 0 {
        return Err(io::Error::last_os_error());
    }

    INTERVAL_NS.store(interval_ns, Ordering::Release);
    DISARM_REQUESTED.store(false, Ordering::Release);
    RUNNING.store(true, Ordering::Release);
    Ok(())
}

/// Disable sampling process-wide. Reversible via `enable`. Per-thread timers
/// stay armed and keep firing SIGPROF, but the handler no-ops while paused.
pub fn disable() {
    RUNNING.store(false, Ordering::Release);
}

/// Re-enable sampling after `disable`.
pub fn enable() {
    RUNNING.store(true, Ordering::Release);
}

/// Requests that all timers are disarmed.
/// A subsequent `start` is required to sample again.
pub fn disarm_all_timers() {
    DISARM_REQUESTED.store(true, Ordering::Release);
    RUNNING.store(false, Ordering::Release);
}

pub fn is_running() -> bool {
    RUNNING.load(Ordering::Acquire)
}

pub fn is_disarm_requested() -> bool {
    DISARM_REQUESTED.load(Ordering::Acquire)
}

pub fn interval_ns() -> i64 {
    INTERVAL_NS.load(Ordering::Relaxed)
}

/// Returns the calling thread's timer handle, or `None` if not registered.
///
/// Called from the SIGPROF handler (same thread as the timer) to pass to
/// `timer_getoverrun` for accurate sample weighting.
pub fn current_thread_timer_id() -> Option<libc::timer_t> {
    THREAD_TIMER.with(|c| c.get())
}

/// Create and arm a per-thread CPU timer for the *calling* thread.
pub fn register_thread() -> Result<(), io::Error> {
    if !RUNNING.load(Ordering::Acquire) {
        return Err(io::Error::other("ctimer is not running (call start first)"));
    }
    let interval = INTERVAL_NS.load(Ordering::Acquire);

    let existing = THREAD_TIMER.with(|c| c.get());
    if existing.is_some() {
        return Ok(());
    }

    let tid = gettid();

    // SAFETY: Zero-filled `libc::sigevent` is valid before we assign the fields we use.
    let mut sev: libc::sigevent = unsafe { mem::zeroed() };
    sev.sigev_notify = libc::SIGEV_THREAD_ID;
    sev.sigev_signo = libc::SIGPROF;
    sev.sigev_notify_thread_id = tid;
    sev.sigev_value = libc::sigval {
        sival_ptr: tid as *mut libc::c_void,
    };

    let mut timerid: libc::timer_t = ptr::null_mut();
    // SAFETY: `sev` and `timerid` are stack locals, libc may read/write them for this syscall only.
    if unsafe { libc::timer_create(libc::CLOCK_THREAD_CPUTIME_ID, &mut sev, &mut timerid) } != 0 {
        return Err(io::Error::last_os_error());
    }

    let sec = interval / 1_000_000_000;
    let nsec = interval % 1_000_000_000;
    let spec = libc::itimerspec {
        it_interval: libc::timespec {
            tv_sec: sec,
            tv_nsec: nsec,
        },
        it_value: libc::timespec {
            tv_sec: sec,
            tv_nsec: nsec,
        },
    };

    // SAFETY: `timerid` is from successful `timer_create`, `spec` is a stack-local reference,
    // `old_value` is null (allowed).
    if unsafe { libc::timer_settime(timerid, 0, &spec, ptr::null_mut()) } != 0 {
        let err = io::Error::last_os_error();
        // Best-effort cleanup on failure.
        // SAFETY: same `timerid`, valid to delete after a failed `timer_settime`.
        if unsafe { libc::timer_delete(timerid) } != 0 {
            let cleanup_err = io::Error::last_os_error();
            tracing::warn!(
                "ctimer: timer_delete after timer_settime failure failed: {cleanup_err}"
            );
        }
        return Err(err);
    }

    THREAD_TIMER.with(|c| c.set(Some(timerid)));
    Ok(())
}

/// Disarm and delete the calling thread's timer. Must run on the thread being
/// unregistered. No-op if never registered.
pub fn unregister_thread() {
    THREAD_TIMER.with(|c| {
        if let Some(t) = c.take() {
            // SAFETY: zero-filled `libc::itimerspec` disarms the timer (all-zero interval and value).
            let zero: libc::itimerspec = unsafe { mem::zeroed() };
            // Best-effort disarm before delete.
            // SAFETY: `t` is a live `timer_t` from this thread's registration, `zero` is stack-local,
            // `old_value` is null (allowed).
            if unsafe { libc::timer_settime(t, 0, &zero, ptr::null_mut()) } != 0 {
                let err = io::Error::last_os_error();
                tracing::warn!("ctimer: timer_settime(disarm) failed in unregister_thread: {err}");
            }
            // SAFETY: `t` is still valid until `timer_delete` succeeds.
            if unsafe { libc::timer_delete(t) } != 0 {
                let err = io::Error::last_os_error();
                tracing::warn!("ctimer: timer_delete failed in unregister_thread: {err}");
            }
        }
    });
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Mutex;
    use std::sync::atomic::AtomicU64;

    // Serializes tests touching process-global state: RUNNING,
    // DISARM_REQUESTED, INTERVAL_NS, and the SIGPROF handler.
    static TEST_LOCK: Mutex<()> = Mutex::new(());

    extern "C" fn dummy_handler(_: libc::c_int, _: *mut libc::siginfo_t, _: *mut libc::c_void) {}

    #[test]
    fn start_rejects_zero_interval() {
        let err = unsafe { start(0, dummy_handler) }.unwrap_err();
        assert_eq!(err.kind(), std::io::ErrorKind::InvalidInput);
    }

    #[test]
    fn start_rejects_negative_interval() {
        let err = unsafe { start(-1, dummy_handler) }.unwrap_err();
        assert_eq!(err.kind(), std::io::ErrorKind::InvalidInput);
    }

    #[test]
    fn register_thread_fails_when_not_running() {
        let _g = TEST_LOCK.lock().unwrap();
        RUNNING.store(false, Ordering::Release);
        let err = register_thread().unwrap_err();
        assert!(err.to_string().contains("not running"));
    }

    #[test]
    fn unregister_thread_is_safe_when_not_registered() {
        THREAD_TIMER.with(|c| c.set(None));
        unregister_thread();
        assert!(THREAD_TIMER.with(|c| c.get()).is_none());
    }

    static SAMPLE_COUNT: AtomicU64 = AtomicU64::new(0);

    extern "C" fn counting_handler(_: libc::c_int, _: *mut libc::siginfo_t, _: *mut libc::c_void) {
        SAMPLE_COUNT.fetch_add(1, Ordering::Relaxed);
    }

    fn thread_cpu_time_ns() -> u64 {
        let mut ts = libc::timespec {
            tv_sec: 0,
            tv_nsec: 0,
        };
        unsafe { libc::clock_gettime(libc::CLOCK_THREAD_CPUTIME_ID, &mut ts) };
        ts.tv_sec as u64 * 1_000_000_000 + ts.tv_nsec as u64
    }

    fn burn_cpu(duration_ns: u64) {
        let deadline = thread_cpu_time_ns() + duration_ns;
        let mut acc: u64 = 0;
        while thread_cpu_time_ns() < deadline {
            for i in 0..10_000u64 {
                acc = acc.wrapping_add(i);
            }
        }
        std::hint::black_box(acc);
    }

    #[test]
    fn register_thread_fires_samples_under_cpu_load() {
        let _g = TEST_LOCK.lock().unwrap();
        SAMPLE_COUNT.store(0, Ordering::Relaxed);

        // ~1000 samples/sec.
        unsafe { start(1_000_000, counting_handler) }.expect("start");
        register_thread().expect("should register thread");

        // 200ms of CPU => expect ~200 samples.
        burn_cpu(200_000_000);

        let count = SAMPLE_COUNT.load(Ordering::Relaxed);

        unregister_thread();

        RUNNING.store(false, Ordering::Release);
        DISARM_REQUESTED.store(false, Ordering::Release);

        // lower threshold to account for noise
        assert!(
            count >= 150,
            "expected >=150 samples from 200ms of CPU at 1kHz, got {count}"
        );
    }
}
