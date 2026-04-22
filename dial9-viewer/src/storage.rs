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
        let root = root.into();
        // Canonicalize root so that symlink resolution in child paths
        // (e.g. macOS /tmp → /private/tmp) matches the root prefix.
        let root = root.canonicalize().unwrap_or(root);
        Self { root }
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
            let root = self.root.clone();
            let prefix2 = prefix.clone();
            tokio::task::spawn_blocking(move || {
                let mut objects = Vec::new();
                collect_files(&root, &root, &prefix2, &mut objects, 0, &mut 0)?;
                objects.sort_by(|a, b| a.key.cmp(&b.key));
                Ok(objects)
            })
            .await
            .map_err(|e| StorageError::Other(e.to_string()))?
        })
    }

    fn list_prefixes(
        &self,
        _bucket: &str,
        prefix: &str,
    ) -> Pin<Box<dyn Future<Output = Result<Vec<String>, StorageError>> + Send + '_>> {
        let prefix = prefix.to_string();
        Box::pin(async move {
            let root = self.root.clone();
            let prefix2 = prefix.clone();
            tokio::task::spawn_blocking(move || {
                let dir = root.join(&prefix2);
                let dir = match dir.canonicalize() {
                    Ok(d) if d.starts_with(&root) => d,
                    Ok(_) => {
                        return Err(StorageError::NotFound(
                            "path escapes root directory".to_string(),
                        ));
                    }
                    Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(vec![]),
                    Err(e) => return Err(StorageError::Other(e.to_string())),
                };
                let entries = match std::fs::read_dir(&dir) {
                    Ok(e) => e,
                    Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(vec![]),
                    Err(e) => return Err(StorageError::Other(e.to_string())),
                };
                let mut prefixes = Vec::new();
                for entry in entries {
                    let entry = entry.map_err(|e| StorageError::Other(e.to_string()))?;
                    let path = entry.path();
                    // Resolve symlinks and verify the target stays within root.
                    let canonical = match path.canonicalize() {
                        Ok(c) if c.starts_with(&root) => c,
                        _ => continue,
                    };
                    if canonical.is_dir() {
                        let name = entry.file_name().to_string_lossy().into_owned();
                        // NOTE: This uses "/" unconditionally, matching S3 key semantics.
                        // On Windows, this would need to use the platform separator or
                        // normalize paths to forward slashes throughout.
                        prefixes.push(format!("{prefix2}{name}/"));
                    }
                }
                prefixes.sort();
                Ok(prefixes)
            })
            .await
            .map_err(|e| StorageError::Other(e.to_string()))?
        })
    }

    fn get_object(
        &self,
        _bucket: &str,
        key: &str,
    ) -> Pin<Box<dyn Future<Output = Result<Vec<u8>, StorageError>> + Send + '_>> {
        let path = self.root.join(key);
        let root = self.root.clone();
        Box::pin(async move {
            tokio::task::spawn_blocking(move || {
                let canonical = path.canonicalize().map_err(|e| match e.kind() {
                    std::io::ErrorKind::NotFound => {
                        StorageError::NotFound(path.display().to_string())
                    }
                    _ => StorageError::Other(e.to_string()),
                })?;
                if !canonical.starts_with(&root) {
                    return Err(StorageError::NotFound(
                        "path escapes root directory".to_string(),
                    ));
                }
                std::fs::read(&canonical).map_err(|e| match e.kind() {
                    std::io::ErrorKind::NotFound => {
                        StorageError::NotFound(path.display().to_string())
                    }
                    _ => StorageError::Other(e.to_string()),
                })
            })
            .await
            .map_err(|e| StorageError::Other(e.to_string()))?
        })
    }
}

/// Maximum directory depth to recurse into when listing local files.
const MAX_COLLECT_DEPTH: u32 = 10;

/// Maximum number of files to return from a local directory listing.
const MAX_COLLECT_FILES: usize = 50;

/// Maximum number of directory entries to visit (files + dirs) across the
/// entire recursive walk. This bounds the number of syscalls (`canonicalize`,
/// `metadata`) so a huge directory tree cannot hang the listing.
const MAX_ENTRIES_VISITED: usize = 10_000;

/// Directory names to skip during recursive file collection.
fn is_skipped_dir(name: &str) -> bool {
    name.starts_with('.') || matches!(name, "target" | "node_modules")
}

fn collect_files(
    root: &Path,
    dir: &Path,
    prefix: &str,
    out: &mut Vec<ObjectInfo>,
    depth: u32,
    visited: &mut usize,
) -> Result<(), StorageError> {
    if depth > MAX_COLLECT_DEPTH
        || out.len() >= MAX_COLLECT_FILES
        || *visited >= MAX_ENTRIES_VISITED
    {
        return Ok(());
    }
    let entries = match std::fs::read_dir(dir) {
        Ok(e) => e,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(()),
        Err(e) if e.kind() == std::io::ErrorKind::PermissionDenied => return Ok(()),
        Err(e) => return Err(StorageError::Other(e.to_string())),
    };
    for entry in entries {
        *visited += 1;
        if out.len() >= MAX_COLLECT_FILES || *visited >= MAX_ENTRIES_VISITED {
            break;
        }
        let entry = entry.map_err(|e| StorageError::Other(e.to_string()))?;
        let path = entry.path();
        // Resolve symlinks and verify the target stays within root.
        let canonical = match path.canonicalize() {
            Ok(c) if c.starts_with(root) => c,
            _ => continue,
        };
        if canonical.is_dir() {
            let name = entry.file_name();
            let name = name.to_string_lossy();
            if !is_skipped_dir(&name) {
                collect_files(root, &canonical, prefix, out, depth + 1, visited)?;
            }
        } else if canonical.is_file() {
            let key = path
                .strip_prefix(root)
                .unwrap_or(&path)
                .to_string_lossy()
                .into_owned();
            if key.starts_with(prefix) {
                let meta = std::fs::metadata(&canonical)
                    .map_err(|e| StorageError::Other(e.to_string()))?;
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn collect_files_caps_entries_visited() {
        let dir = tempfile::tempdir().unwrap();
        // Create more files than MAX_ENTRIES_VISITED to prove we stop early.
        let n = MAX_ENTRIES_VISITED + 500;
        for i in 0..n {
            std::fs::write(dir.path().join(format!("file_{i:05}.bin")), b"x").unwrap();
        }
        let mut out = Vec::new();
        let mut visited = 0;
        collect_files(dir.path(), dir.path(), "", &mut out, 0, &mut visited).unwrap();
        // visited must be capped — we should NOT have iterated all n files.
        assert!(
            visited <= MAX_ENTRIES_VISITED,
            "visited {visited} entries, expected at most {MAX_ENTRIES_VISITED}"
        );
        assert!(
            out.len() <= MAX_COLLECT_FILES,
            "collected {} files, expected at most {MAX_COLLECT_FILES}",
            out.len()
        );
    }
}
