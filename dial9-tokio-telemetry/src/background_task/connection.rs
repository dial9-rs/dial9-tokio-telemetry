//! Circuit breaker for S3 uploads.
//!
//! Tracks whether S3 is reachable and manages exponential backoff
//! when uploads fail. Prevents spamming S3 with requests when it's
//! unreachable — the SDK's built-in retries handle per-request retries,
//! while this provides higher-level circuit-breaking across requests.

use std::time::{Duration, Instant};

const INITIAL_BACKOFF: Duration = Duration::from_secs(1);
const MAX_BACKOFF: Duration = Duration::from_secs(300); // 5 minutes

/// Circuit breaker for S3 upload attempts.
#[derive(Debug, Default, PartialEq, Eq)]
pub(crate) enum CircuitBreaker {
    /// S3 is reachable. Normal upload + delete.
    #[default]
    Closed,
    /// S3 is unreachable. Skip uploads, retry with backoff.
    Open {
        next_retry: Instant,
        backoff: Duration,
    },
}

impl CircuitBreaker {
    /// Create a new closed (healthy) circuit breaker.
    pub(crate) fn new() -> Self {
        Self::Closed
    }

    /// Whether uploads should be attempted right now.
    pub(crate) fn should_attempt(&self) -> bool {
        match self {
            Self::Closed => true,
            Self::Open { next_retry, .. } => Instant::now() >= *next_retry,
        }
    }

    /// Record a successful upload. Closes the circuit.
    pub(crate) fn on_success(&mut self) {
        *self = Self::Closed;
    }

    /// Record a failed upload. Opens the circuit with exponential backoff.
    pub(crate) fn on_failure(&mut self) {
        let backoff = match self {
            Self::Closed => INITIAL_BACKOFF,
            Self::Open { backoff, .. } => (*backoff * 2).min(MAX_BACKOFF),
        };
        *self = Self::Open {
            next_retry: Instant::now() + backoff,
            backoff,
        };
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use assert2::check;
    use std::time::Duration;

    /// Extract the backoff duration from an open circuit breaker.
    fn backoff_of(cb: &CircuitBreaker) -> Duration {
        match cb {
            CircuitBreaker::Open { backoff, .. } => *backoff,
            CircuitBreaker::Closed => panic!("expected Open, got Closed"),
        }
    }

    #[test]
    fn starts_closed() {
        let cb = CircuitBreaker::new();
        check!(cb == CircuitBreaker::Closed);
        check!(cb.should_attempt());
    }

    #[test]
    fn opens_on_failure() {
        let mut cb = CircuitBreaker::new();
        cb.on_failure();
        check!(cb != CircuitBreaker::Closed);
        check!(backoff_of(&cb) == Duration::from_secs(1));
    }

    #[test]
    fn closes_on_success() {
        let mut cb = CircuitBreaker::new();
        cb.on_failure();
        cb.on_success();
        check!(cb == CircuitBreaker::Closed);
        check!(cb.should_attempt());
    }

    #[test]
    fn backoff_doubles_on_repeated_failures() {
        let mut cb = CircuitBreaker::new();
        cb.on_failure();
        check!(backoff_of(&cb) == Duration::from_secs(1));
        cb.on_failure();
        check!(backoff_of(&cb) == Duration::from_secs(2));
        cb.on_failure();
        check!(backoff_of(&cb) == Duration::from_secs(4));
    }

    #[test]
    fn backoff_caps_at_max() {
        let mut cb = CircuitBreaker::new();
        for _ in 0..20 {
            cb.on_failure();
        }
        check!(backoff_of(&cb) == Duration::from_secs(300));
    }

    #[test]
    fn success_resets_backoff() {
        let mut cb = CircuitBreaker::new();
        cb.on_failure();
        cb.on_failure();
        cb.on_success();
        check!(cb == CircuitBreaker::Closed);
        cb.on_failure();
        check!(backoff_of(&cb) == Duration::from_secs(1));
    }
}
