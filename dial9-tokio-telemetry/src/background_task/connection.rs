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
#[derive(Debug, Default)]
pub enum CircuitBreaker {
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
    pub fn new() -> Self {
        Self::Closed
    }

    /// Whether uploads should be attempted right now.
    pub fn should_attempt(&self) -> bool {
        match self {
            Self::Closed => true,
            Self::Open { next_retry, .. } => Instant::now() >= *next_retry,
        }
    }

    /// Record a successful upload. Closes the circuit.
    pub fn on_success(&mut self) {
        *self = Self::Closed;
    }

    /// Record a failed upload. Opens the circuit with exponential backoff.
    pub fn on_failure(&mut self) {
        let backoff = match self {
            Self::Closed => INITIAL_BACKOFF,
            Self::Open { backoff, .. } => (*backoff * 2).min(MAX_BACKOFF),
        };
        *self = Self::Open {
            next_retry: Instant::now() + backoff,
            backoff,
        };
    }

    /// Whether the circuit is currently closed (healthy).
    pub fn is_closed(&self) -> bool {
        matches!(self, Self::Closed)
    }

    /// Current backoff duration (for testing/logging). None if closed.
    pub fn current_backoff(&self) -> Option<Duration> {
        match self {
            Self::Closed => None,
            Self::Open { backoff, .. } => Some(*backoff),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use assert2::check;
    use std::time::Duration;

    #[test]
    fn starts_closed() {
        let cb = CircuitBreaker::new();
        check!(cb.is_closed());
        check!(cb.should_attempt());
    }

    #[test]
    fn opens_on_failure() {
        let mut cb = CircuitBreaker::new();
        cb.on_failure();
        check!(!cb.is_closed());
        check!(cb.current_backoff() == Some(Duration::from_secs(1)));
    }

    #[test]
    fn closes_on_success() {
        let mut cb = CircuitBreaker::new();
        cb.on_failure();
        cb.on_success();
        check!(cb.is_closed());
        check!(cb.should_attempt());
    }

    #[test]
    fn backoff_doubles_on_repeated_failures() {
        let mut cb = CircuitBreaker::new();
        cb.on_failure();
        check!(cb.current_backoff() == Some(Duration::from_secs(1)));
        cb.on_failure();
        check!(cb.current_backoff() == Some(Duration::from_secs(2)));
        cb.on_failure();
        check!(cb.current_backoff() == Some(Duration::from_secs(4)));
    }

    #[test]
    fn backoff_caps_at_max() {
        let mut cb = CircuitBreaker::new();
        for _ in 0..20 {
            cb.on_failure();
        }
        check!(cb.current_backoff() == Some(Duration::from_secs(300)));
    }

    #[test]
    fn success_resets_backoff() {
        let mut cb = CircuitBreaker::new();
        cb.on_failure();
        cb.on_failure();
        cb.on_success();
        check!(cb.is_closed());
        check!(cb.current_backoff() == None);
        cb.on_failure();
        check!(cb.current_backoff() == Some(Duration::from_secs(1)));
    }
}
