//! Rate limiting for log lines.
//!
//! Adapted from metrique-writer-format-emf's rate_limit module.

use std::sync::OnceLock;
use std::time::{Duration, Instant};

#[doc(hidden)]
pub(crate) fn time_since_epoch() -> Duration {
    static EPOCH: OnceLock<Instant> = OnceLock::new();
    Instant::now().duration_since(*EPOCH.get_or_init(Instant::now))
}

/// Evaluate `$call` at most once every `$interval` per call site.
macro_rules! rate_limited {
    ($interval:expr, $call:expr) => {{
        use std::sync::atomic::{AtomicU64, Ordering};
        static NEXT_CALL: AtomicU64 = AtomicU64::new(u64::MIN);
        let interval = $interval;
        let time = $crate::rate_limit::time_since_epoch();
        let next = NEXT_CALL.load(Ordering::Relaxed);
        if next <= time.as_secs() {
            let new_next = time
                .checked_add(interval)
                .unwrap_or(Duration::MAX)
                .as_secs();
            if NEXT_CALL
                .compare_exchange(next, new_next, Ordering::Relaxed, Ordering::Relaxed)
                .is_ok()
            {
                $call;
            }
        }
    }};
}

pub(crate) use rate_limited;
