pub mod sealed;
#[cfg(feature = "worker")]
pub mod identity;
#[cfg(feature = "worker-s3")]
pub mod connection;
#[cfg(feature = "worker-s3")]
pub mod s3;

#[cfg(feature = "worker")]
mod worker_config {
    use std::time::Duration;

    const DEFAULT_POLL_INTERVAL: Duration = Duration::from_secs(1);

    /// Configuration for the in-process worker pipeline.
    pub struct WorkerConfig {
        poll_interval: Duration,
        #[cfg(feature = "worker-s3")]
        s3: Option<super::s3::S3Config>,
    }

    impl WorkerConfig {
        pub fn builder() -> WorkerConfigBuilder {
            WorkerConfigBuilder::default()
        }

        /// How often the worker checks for sealed segments.
        pub fn poll_interval(&self) -> Duration {
            self.poll_interval
        }

        /// S3 upload configuration, if any.
        #[cfg(feature = "worker-s3")]
        pub fn s3(&self) -> Option<&super::s3::S3Config> {
            self.s3.as_ref()
        }
    }

    #[derive(Default)]
    pub struct WorkerConfigBuilder {
        poll_interval: Option<Duration>,
        #[cfg(feature = "worker-s3")]
        s3: Option<super::s3::S3Config>,
    }

    impl WorkerConfigBuilder {
        /// Set the poll interval for checking sealed segments.
        /// Defaults to 1 second.
        pub fn poll_interval(mut self, interval: Duration) -> Self {
            self.poll_interval = Some(interval);
            self
        }

        /// Set S3 upload configuration.
        #[cfg(feature = "worker-s3")]
        pub fn s3(mut self, config: super::s3::S3Config) -> Self {
            self.s3 = Some(config);
            self
        }

        pub fn build(self) -> WorkerConfig {
            WorkerConfig {
                poll_interval: self.poll_interval.unwrap_or(DEFAULT_POLL_INTERVAL),
                #[cfg(feature = "worker-s3")]
                s3: self.s3,
            }
        }
    }
}

#[cfg(feature = "worker")]
pub use worker_config::*;
