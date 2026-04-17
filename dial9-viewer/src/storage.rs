use serde::{Deserialize, Serialize};
use std::future::Future;
use std::path::{Path, PathBuf};
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

    /// List immediate child prefixes under `prefix` using delimiter-based listing.
    fn list_prefixes(
        &self,
        bucket: &str,
        prefix: &str,
    ) -> Pin<Box<dyn Future<Output = Result<Vec<String>, StorageError>> + Send + '_>>;

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

    fn list_prefixes(
        &self,
        bucket: &str,
        prefix: &str,
    ) -> Pin<Box<dyn Future<Output = Result<Vec<String>, StorageError>> + Send + '_>> {
        let bucket = bucket.to_string();
        let prefix = prefix.to_string();
        Box::pin(async move {
            let resp = self
                .client
                .list_objects_v2()
                .bucket(&bucket)
                .prefix(&prefix)
                .delimiter("/")
                .send()
                .await
                .map_err(|e| {
                    use aws_sdk_s3::error::DisplayErrorContext;
                    StorageError::Other(format!("{}", DisplayErrorContext(&e)))
                })?;

            Ok(resp
                .common_prefixes()
                .iter()
                .filter_map(|cp| cp.prefix().map(|s| s.to_string()))
                .collect())
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

/// Local filesystem storage backend. Serves trace files from a directory.
///
/// The `bucket` parameter is ignored — all operations are relative to `root`.
/// Keys are relative paths from `root`.
pub struct LocalBackend {
    root: PathBuf,
}

impl LocalBackend {
    pub fn new(root: impl Into<PathBuf>) -> Self {
        Self { root: root.into() }
    }
}

impl StorageBackend for LocalBackend {
    fn list_objects(
        &self,
        _bucket: &str,
        prefix: &str,
    ) -> Pin<Box<dyn Future<Output = Result<Vec<ObjectInfo>, StorageError>> + Send + '_>> {
        let prefix = prefix.to_string();
        Box::pin(async move {
            let mut objects = Vec::new();
            collect_files(&self.root, &self.root, &prefix, &mut objects)?;
            objects.sort_by(|a, b| a.key.cmp(&b.key));
            Ok(objects)
        })
    }

    fn list_prefixes(
        &self,
        _bucket: &str,
        prefix: &str,
    ) -> Pin<Box<dyn Future<Output = Result<Vec<String>, StorageError>> + Send + '_>> {
        let prefix = prefix.to_string();
        Box::pin(async move {
            let dir = self.root.join(&prefix);
            let entries = match std::fs::read_dir(&dir) {
                Ok(e) => e,
                Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(vec![]),
                Err(e) => return Err(StorageError::Other(e.to_string())),
            };
            let mut prefixes = Vec::new();
            for entry in entries {
                let entry = entry.map_err(|e| StorageError::Other(e.to_string()))?;
                if entry
                    .file_type()
                    .map_err(|e| StorageError::Other(e.to_string()))?
                    .is_dir()
                {
                    let name = entry.file_name().to_string_lossy().into_owned();
                    prefixes.push(format!("{prefix}{name}/"));
                }
            }
            prefixes.sort();
            Ok(prefixes)
        })
    }

    fn get_object(
        &self,
        _bucket: &str,
        key: &str,
    ) -> Pin<Box<dyn Future<Output = Result<Vec<u8>, StorageError>> + Send + '_>> {
        let path = self.root.join(key);
        Box::pin(async move {
            std::fs::read(&path).map_err(|e| match e.kind() {
                std::io::ErrorKind::NotFound => StorageError::NotFound(path.display().to_string()),
                _ => StorageError::Other(e.to_string()),
            })
        })
    }
}

fn collect_files(
    root: &Path,
    dir: &Path,
    prefix: &str,
    out: &mut Vec<ObjectInfo>,
) -> Result<(), StorageError> {
    let entries = match std::fs::read_dir(dir) {
        Ok(e) => e,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(()),
        Err(e) => return Err(StorageError::Other(e.to_string())),
    };
    for entry in entries {
        let entry = entry.map_err(|e| StorageError::Other(e.to_string()))?;
        let path = entry.path();
        let ft = entry
            .file_type()
            .map_err(|e| StorageError::Other(e.to_string()))?;
        if ft.is_dir() {
            collect_files(root, &path, prefix, out)?;
        } else if ft.is_file() {
            let key = path
                .strip_prefix(root)
                .unwrap_or(&path)
                .to_string_lossy()
                .into_owned();
            if key.starts_with(prefix) {
                let meta =
                    std::fs::metadata(&path).map_err(|e| StorageError::Other(e.to_string()))?;
                out.push(ObjectInfo {
                    key,
                    size: meta.len() as i64,
                    last_modified: meta.modified().ok().and_then(|t| {
                        t.duration_since(std::time::UNIX_EPOCH)
                            .ok()
                            .map(|d| d.as_secs().to_string())
                    }),
                });
            }
        }
    }
    Ok(())
}
