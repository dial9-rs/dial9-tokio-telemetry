use serde::{Deserialize, Serialize};
use std::future::Future;
use std::pin::Pin;

/// Metadata about an object in storage.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ObjectInfo {
    pub key: String,
    pub size: i64,
    pub last_modified: Option<String>,
}

/// Abstraction over trace storage (S3, local FS, etc.)
pub trait StorageBackend: Send + Sync {
    fn list_objects(
        &self,
        bucket: &str,
        prefix: &str,
    ) -> Pin<Box<dyn Future<Output = Result<Vec<ObjectInfo>, StorageError>> + Send + '_>>;

    fn get_object(
        &self,
        bucket: &str,
        key: &str,
    ) -> Pin<Box<dyn Future<Output = Result<Vec<u8>, StorageError>> + Send + '_>>;
}

#[derive(Debug)]
pub enum StorageError {
    NotFound(String),
    Other(String),
}

impl std::fmt::Display for StorageError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            StorageError::NotFound(msg) => write!(f, "not found: {msg}"),
            StorageError::Other(msg) => write!(f, "{msg}"),
        }
    }
}

impl std::error::Error for StorageError {}

/// S3-backed storage using the AWS SDK.
pub struct S3Backend {
    client: aws_sdk_s3::Client,
}

impl S3Backend {
    pub async fn from_env() -> Self {
        let config = aws_config::load_defaults(aws_config::BehaviorVersion::latest()).await;
        Self {
            client: aws_sdk_s3::Client::new(&config),
        }
    }

    /// Create from an existing S3 client (useful for testing with s3s).
    pub fn from_client(client: aws_sdk_s3::Client) -> Self {
        Self { client }
    }
}

impl StorageBackend for S3Backend {
    fn list_objects(
        &self,
        bucket: &str,
        prefix: &str,
    ) -> Pin<Box<dyn Future<Output = Result<Vec<ObjectInfo>, StorageError>> + Send + '_>> {
        let bucket = bucket.to_string();
        let prefix = prefix.to_string();
        Box::pin(async move {
            const MAX_RESULTS: usize = 1000;
            let mut objects = Vec::new();
            let mut continuation: Option<String> = None;

            loop {
                let mut req = self
                    .client
                    .list_objects_v2()
                    .bucket(&bucket)
                    .prefix(&prefix);
                if let Some(token) = continuation.take() {
                    req = req.continuation_token(token);
                }

                let resp = req.send().await.map_err(|e| {
                    use aws_sdk_s3::error::DisplayErrorContext;
                    StorageError::Other(format!("{}", DisplayErrorContext(&e)))
                })?;

                for obj in resp.contents() {
                    if let Some(key) = obj.key() {
                        objects.push(ObjectInfo {
                            key: key.to_string(),
                            size: obj.size().unwrap_or(0),
                            last_modified: obj.last_modified().map(|t| t.to_string()),
                        });
                    }
                }

                if objects.len() >= MAX_RESULTS {
                    objects.truncate(MAX_RESULTS);
                    break;
                }

                if resp.is_truncated() == Some(true) {
                    continuation = resp.next_continuation_token().map(|s| s.to_string());
                } else {
                    break;
                }
            }

            Ok(objects)
        })
    }

    fn get_object(
        &self,
        bucket: &str,
        key: &str,
    ) -> Pin<Box<dyn Future<Output = Result<Vec<u8>, StorageError>> + Send + '_>> {
        let bucket = bucket.to_string();
        let key = key.to_string();
        Box::pin(async move {
            let resp = self
                .client
                .get_object()
                .bucket(&bucket)
                .key(&key)
                .send()
                .await
                .map_err(|e| {
                    use aws_sdk_s3::error::DisplayErrorContext;
                    use aws_sdk_s3::operation::get_object::GetObjectError;

                    match e.into_service_error() {
                        GetObjectError::NoSuchKey(_) => {
                            StorageError::NotFound(format!("{bucket}/{key}"))
                        }
                        other => StorageError::Other(format!("{}", DisplayErrorContext(&other))),
                    }
                })?;

            let bytes = resp
                .body
                .collect()
                .await
                .map_err(|e| StorageError::Other(e.to_string()))?;

            Ok(bytes.to_vec())
        })
    }
}
