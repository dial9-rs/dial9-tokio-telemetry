//! Machine identity detection for S3 object key paths.
//!
//! Provides `InstanceIdentity` — a newtype around `String` used as the
//! `instance_path` component in S3 object keys.

/// Identifies where a process is running, used as the `instance_path`
/// component in S3 object keys.
///
/// # Examples
/// ```
/// use dial9_tokio_telemetry::background_task::instance_metadata::InstanceIdentity;
///
/// let id = InstanceIdentity::new("us-east-1/i-0abc123");
/// assert_eq!(id.as_str(), "us-east-1/i-0abc123");
/// ```
#[derive(Clone)]
pub struct InstanceIdentity(String);

impl From<String> for InstanceIdentity {
    fn from(s: String) -> Self {
        Self(s)
    }
}

impl From<&str> for InstanceIdentity {
    fn from(s: &str) -> Self {
        Self(s.to_owned())
    }
}

impl InstanceIdentity {
    /// Create an identity from an explicit string.
    pub fn new(s: impl Into<String>) -> Self {
        Self(s.into())
    }

    /// Auto-detect identity from the system hostname.
    pub fn from_hostname() -> Self {
        let hostname = hostname::get()
            .ok()
            .and_then(|h| h.into_string().ok())
            .unwrap_or_else(|| "unknown".to_string());
        Self(hostname)
    }

    /// The identity string.
    pub fn as_str(&self) -> &str {
        &self.0
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use assert2::check;

    #[test]
    fn from_hostname_returns_non_empty_string() {
        let id = InstanceIdentity::from_hostname();
        check!(!id.as_str().is_empty());
    }
}
