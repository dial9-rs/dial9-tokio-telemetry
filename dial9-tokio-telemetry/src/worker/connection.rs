//! S3 connection state machine.
//!
//! Tracks whether S3 is reachable and manages exponential backoff
//! when uploads fail. See design doc §9.2.

#[cfg(feature = "worker-s3")]
mod inner {
    use std::time::{Duration, Instant};

    const INITIAL_BACKOFF: Duration = Duration::from_secs(1);
    const MAX_BACKOFF: Duration = Duration::from_secs(300); // 5 minutes

    /// S3 connection health state.
    #[derive(Debug)]
    pub enum S3ConnectionState {
        /// S3 is reachable. Normal upload + delete.
        Healthy,
        /// S3 is unreachable. Skip uploads, retry with backoff.
        Degraded {
            next_retry: Instant,
            backoff: Duration,
        },
    }

    impl S3ConnectionState {
        /// Create a new healthy state.
        pub fn healthy() -> Self {
            Self::Healthy
        }

        /// Whether uploads should be attempted right now.
        pub fn should_attempt_upload(&self) -> bool {
            match self {
                Self::Healthy => true,
                Self::Degraded { next_retry, .. } => Instant::now() >= *next_retry,
            }
        }

        /// Record a successful upload or validation. Transitions to Healthy.
        pub fn on_success(&mut self) {
            *self = Self::Healthy;
        }

        /// Record a failed upload or validation. Transitions to Degraded
        /// with exponential backoff.
        pub fn on_failure(&mut self) {
            let backoff = match self {
                Self::Healthy => INITIAL_BACKOFF,
                Self::Degraded { backoff, .. } => (*backoff * 2).min(MAX_BACKOFF),
            };
            *self = Self::Degraded {
                next_retry: Instant::now() + backoff,
                backoff,
            };
        }

        /// Whether the connection is currently healthy.
        pub fn is_healthy(&self) -> bool {
            matches!(self, Self::Healthy)
        }

        /// Current backoff duration (for testing/logging). None if healthy.
        pub fn current_backoff(&self) -> Option<Duration> {
            match self {
                Self::Healthy => None,
                Self::Degraded { backoff, .. } => Some(*backoff),
            }
        }
    }
}

#[cfg(feature = "worker-s3")]
pub use inner::*;

#[cfg(test)]
#[cfg(feature = "worker-s3")]
mod tests {
    use super::*;
    use std::time::Duration;

    #[test]
    fn starts_healthy() {
        let state = S3ConnectionState::healthy();
        assert!(state.is_healthy());
        assert!(state.should_attempt_upload());
        assert!(state.current_backoff().is_none());
    }

    #[test]
    fn failure_transitions_to_degraded() {
        let mut state = S3ConnectionState::healthy();
        state.on_failure();
        assert!(!state.is_healthy());
        assert_eq!(state.current_backoff(), Some(Duration::from_secs(1)));
    }

    #[test]
    fn success_transitions_back_to_healthy() {
        let mut state = S3ConnectionState::healthy();
        state.on_failure();
        assert!(!state.is_healthy());
        state.on_success();
        assert!(state.is_healthy());
    }

    #[test]
    fn backoff_doubles_on_repeated_failures() {
        let mut state = S3ConnectionState::healthy();
        state.on_failure();
        assert_eq!(state.current_backoff(), Some(Duration::from_secs(1)));
        state.on_failure();
        assert_eq!(state.current_backoff(), Some(Duration::from_secs(2)));
        state.on_failure();
        assert_eq!(state.current_backoff(), Some(Duration::from_secs(4)));
    }

    #[test]
    fn backoff_caps_at_five_minutes() {
        let mut state = S3ConnectionState::healthy();
        // Drive backoff past the cap: 1, 2, 4, 8, 16, 32, 64, 128, 256, 300, 300
        for _ in 0..20 {
            state.on_failure();
        }
        assert_eq!(state.current_backoff(), Some(Duration::from_secs(300)));
    }

    #[test]
    fn recovery_resets_backoff() {
        let mut state = S3ConnectionState::healthy();
        state.on_failure();
        state.on_failure();
        state.on_failure();
        assert_eq!(state.current_backoff(), Some(Duration::from_secs(4)));

        state.on_success();
        assert!(state.is_healthy());
        assert!(state.current_backoff().is_none());

        // Next failure starts from initial backoff again
        state.on_failure();
        assert_eq!(state.current_backoff(), Some(Duration::from_secs(1)));
    }

    #[test]
    fn degraded_blocks_upload_until_retry_time() {
        let mut state = S3ConnectionState::healthy();
        state.on_failure();
        // Just failed — next_retry is ~1s in the future, so should_attempt_upload is false
        assert!(!state.should_attempt_upload());
    }
}
