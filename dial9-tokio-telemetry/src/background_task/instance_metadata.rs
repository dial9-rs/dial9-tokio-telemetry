//! Machine identity detection for S3 object key paths.

/// Identifies where a process is running, used as the `instance_path`
/// component in S3 object keys.
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
