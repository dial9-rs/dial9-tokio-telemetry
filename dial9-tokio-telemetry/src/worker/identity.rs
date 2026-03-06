//! Machine identity detection for S3 object key paths.
//!
//! Provides `InstanceIdentity` which resolves to a string used as the
//! `instance_path` component in S3 object keys. Can be an explicit string
//! or auto-detected from the hostname.

/// Identifies where a process is running, used as the `instance_path`
/// component in S3 object keys.
///
/// # Examples
/// ```
/// use dial9_tokio_telemetry::worker::identity::InstanceIdentity;
///
/// // Explicit string
/// let id = InstanceIdentity::explicit("us-east-1/i-0abc123");
/// assert_eq!(id.resolve(), "us-east-1/i-0abc123");
/// ```
pub enum InstanceIdentity {
    /// User-provided identity string (e.g. "us-east-1/i-0abc123").
    Explicit(String),
    /// Auto-detected from hostname.
    Hostname(String),
}

impl InstanceIdentity {
    /// Create an identity from an explicit string.
    pub fn explicit(s: impl Into<String>) -> Self {
        Self::Explicit(s.into())
    }

    /// Auto-detect identity from the system hostname.
    pub fn from_hostname() -> Self {
        let hostname = hostname::get()
            .ok()
            .and_then(|h| h.into_string().ok())
            .unwrap_or_else(|| "unknown".to_string());
        Self::Hostname(hostname)
    }

    /// Resolve the identity to a string suitable for use in S3 key paths.
    pub fn resolve(&self) -> &str {
        match self {
            Self::Explicit(s) => s,
            Self::Hostname(h) => h,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn explicit_identity_resolves_to_given_string() {
        let id = InstanceIdentity::explicit("us-east-1/i-0abc123");
        assert_eq!(id.resolve(), "us-east-1/i-0abc123");
    }

    #[test]
    fn explicit_identity_accepts_into_string() {
        let id = InstanceIdentity::explicit(String::from("dc-west/rack4"));
        assert_eq!(id.resolve(), "dc-west/rack4");
    }

    #[test]
    fn from_hostname_returns_non_empty_string() {
        let id = InstanceIdentity::from_hostname();
        assert!(!id.resolve().is_empty());
    }

    #[test]
    fn from_hostname_does_not_return_unknown_on_real_system() {
        // On any real system, gethostname should succeed
        let id = InstanceIdentity::from_hostname();
        assert_ne!(id.resolve(), "unknown");
    }
}
