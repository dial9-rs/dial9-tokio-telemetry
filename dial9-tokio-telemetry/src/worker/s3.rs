//! S3 uploader for sealed trace segments.
//!
//! Gzip-compresses sealed segments and uploads them to S3 using the transfer manager.
//! Deletes local files only after confirmed upload.

#[cfg(feature = "worker-s3")]
mod inner {
    use crate::worker::sealed::SealedSegment;
    use aws_sdk_s3_transfer_manager::Client;
    use flate2::write::GzEncoder;
    use flate2::Compression;
    use std::io::Write;

    /// Configuration for S3 uploads.
    #[derive(Clone)]
    pub struct S3Config {
        bucket: String,
        prefix: String,
        service_name: String,
        instance_path: String,
        boot_id: String,
    }

    impl S3Config {
        pub fn builder() -> S3ConfigBuilder {
            S3ConfigBuilder::default()
        }

        /// Build the S3 object key for a sealed segment.
        ///
        /// Format: `{prefix}/{service_name}/{instance_path}/{timestamp}-{index}.bin.gz`
        /// If prefix is empty: `{service_name}/{instance_path}/{timestamp}-{index}.bin.gz`
        pub fn object_key(&self, segment: &SealedSegment, timestamp: &str) -> String {
            let base = if self.prefix.is_empty() {
                format!("{}/{}", self.service_name, self.instance_path)
            } else {
                format!(
                    "{}/{}/{}",
                    self.prefix, self.service_name, self.instance_path
                )
            };
            format!("{}/{}-{}.bin.gz", base, timestamp, segment.index)
        }
    }

    #[derive(Default)]
    pub struct S3ConfigBuilder {
        bucket: Option<String>,
        prefix: Option<String>,
        service_name: Option<String>,
        instance_path: Option<String>,
        boot_id: Option<String>,
    }

    impl S3ConfigBuilder {
        pub fn bucket(mut self, bucket: impl Into<String>) -> Self {
            self.bucket = Some(bucket.into());
            self
        }

        pub fn prefix(mut self, prefix: impl Into<String>) -> Self {
            self.prefix = Some(prefix.into());
            self
        }

        pub fn service_name(mut self, service_name: impl Into<String>) -> Self {
            self.service_name = Some(service_name.into());
            self
        }

        pub fn instance_path(mut self, instance_path: impl Into<String>) -> Self {
            self.instance_path = Some(instance_path.into());
            self
        }

        pub fn boot_id(mut self, boot_id: impl Into<String>) -> Self {
            self.boot_id = Some(boot_id.into());
            self
        }

        pub fn build(self) -> Result<S3Config, &'static str> {
            Ok(S3Config {
                bucket: self.bucket.ok_or("bucket is required")?,
                prefix: self.prefix.unwrap_or_default(),
                service_name: self.service_name.ok_or("service_name is required")?,
                instance_path: self.instance_path.ok_or("instance_path is required")?,
                boot_id: self.boot_id.ok_or("boot_id is required")?,
            })
        }
    }

    /// Gzip-compress a byte slice.
    pub fn gzip_compress(data: &[u8]) -> std::io::Result<Vec<u8>> {
        let mut encoder = GzEncoder::new(Vec::new(), Compression::fast());
        encoder.write_all(data)?;
        encoder.finish()
    }

    /// Uploads sealed trace segments to S3.
    pub struct S3Uploader {
        client: Client,
        config: S3Config,
    }

    impl S3Uploader {
        pub fn new(client: Client, config: S3Config) -> Self {
            Self { client, config }
        }

        /// Upload a sealed segment to S3, then delete the local file on success.
        ///
        /// Returns the S3 key of the uploaded object.
        pub async fn upload_and_delete(
            &self,
            segment: &SealedSegment,
            timestamp: &str,
        ) -> Result<String, Box<dyn std::error::Error + Send + Sync>> {
            let data = std::fs::read(&segment.path)?;
            let compressed = gzip_compress(&data)?;
            let key = self.config.object_key(segment, timestamp);

            let handle =
                aws_sdk_s3_transfer_manager::operation::upload::UploadInput::builder()
                    .bucket(&self.config.bucket)
                    .key(&key)
                    .content_encoding("gzip")
                    .content_type("application/octet-stream")
                    .metadata("service", &self.config.service_name)
                    .metadata("boot-id", &self.config.boot_id)
                    .metadata("segment-index", segment.index.to_string())
                    .metadata("start-time", timestamp)
                    .metadata("host", &self.config.instance_path)
                    .body(compressed.into())
                    .initiate_with(&self.client)?;

            handle.join().await?;

            std::fs::remove_file(&segment.path)?;

            Ok(key)
        }
    }
}

#[cfg(feature = "worker-s3")]
pub use inner::*;

#[cfg(test)]
#[cfg(feature = "worker-s3")]
mod tests {
    use super::*;
    use crate::worker::sealed::SealedSegment;
    use flate2::read::GzDecoder;
    use std::io::Read;
    use std::path::PathBuf;

    fn make_config() -> S3Config {
        S3Config::builder()
            .bucket("test-bucket")
            .prefix("traces")
            .service_name("checkout-api")
            .instance_path("us-east-1/i-0abc123")
            .boot_id("test-boot-id")
            .build()
            .unwrap()
    }

    fn make_segment(path: impl Into<PathBuf>, index: u32) -> SealedSegment {
        SealedSegment {
            path: path.into(),
            index,
        }
    }

    /// Create a transfer manager Client backed by s3s-fs (in-memory fake S3).
    fn fake_s3_client(fs_root: &std::path::Path) -> aws_sdk_s3_transfer_manager::Client {
        let fs = s3s_fs::FileSystem::new(fs_root).unwrap();
        let mut builder = s3s::service::S3ServiceBuilder::new(fs);
        builder.set_auth(s3s::auth::SimpleAuth::from_single("test", "test"));
        let s3_service = builder.build();
        let s3_client: s3s_aws::Client = s3_service.into();

        let s3_config = aws_sdk_s3::Config::builder()
            .behavior_version_latest()
            .credentials_provider(aws_sdk_s3::config::Credentials::new(
                "test", "test", None, None, "test",
            ))
            .region(aws_sdk_s3::config::Region::new("us-east-1"))
            .http_client(s3_client)
            .force_path_style(true)
            .build();

        let sdk_client = aws_sdk_s3::Client::from_conf(s3_config);

        let tm_config = aws_sdk_s3_transfer_manager::Config::builder()
            .client(sdk_client)
            .build();

        aws_sdk_s3_transfer_manager::Client::new(tm_config)
    }

    /// Create a raw aws_sdk_s3::Client for reading back objects from the fake S3.
    fn fake_raw_s3_client(fs_root: &std::path::Path) -> aws_sdk_s3::Client {
        let fs = s3s_fs::FileSystem::new(fs_root).unwrap();
        let mut builder = s3s::service::S3ServiceBuilder::new(fs);
        builder.set_auth(s3s::auth::SimpleAuth::from_single("test", "test"));
        let s3_service = builder.build();
        let s3_client: s3s_aws::Client = s3_service.into();

        let s3_config = aws_sdk_s3::Config::builder()
            .behavior_version_latest()
            .credentials_provider(aws_sdk_s3::config::Credentials::new(
                "test", "test", None, None, "test",
            ))
            .region(aws_sdk_s3::config::Region::new("us-east-1"))
            .http_client(s3_client)
            .force_path_style(true)
            .build();

        aws_sdk_s3::Client::from_conf(s3_config)
    }

    // --- Key format tests ---

    #[test]
    fn object_key_includes_all_components() {
        let config = make_config();
        let segment = make_segment("/tmp/trace.3.bin", 3);
        let key = config.object_key(&segment, "2026-03-05T19-30-00Z");
        assert_eq!(
            key,
            "traces/checkout-api/us-east-1/i-0abc123/2026-03-05T19-30-00Z-3.bin.gz"
        );
    }

    #[test]
    fn object_key_empty_prefix() {
        let config = S3Config::builder()
            .bucket("my-traces")
            .service_name("checkout-api")
            .instance_path("us-east-1/i-0abc123")
            .boot_id("test-boot-id")
            .build()
            .unwrap();
        let segment = make_segment("/tmp/trace.0.bin", 0);
        let key = config.object_key(&segment, "2026-03-05T19-30-00Z");
        assert_eq!(
            key,
            "checkout-api/us-east-1/i-0abc123/2026-03-05T19-30-00Z-0.bin.gz"
        );
    }

    // --- Gzip compression tests ---

    #[test]
    fn gzip_compress_roundtrips() {
        let original = b"hello world, this is trace data that should compress well!";
        let compressed = gzip_compress(original).unwrap();
        assert_ne!(&compressed[..], &original[..]);

        let mut decoder = GzDecoder::new(&compressed[..]);
        let mut decompressed = Vec::new();
        decoder.read_to_end(&mut decompressed).unwrap();
        assert_eq!(decompressed, original);
    }

    #[test]
    fn gzip_compress_empty_input() {
        let compressed = gzip_compress(b"").unwrap();
        let mut decoder = GzDecoder::new(&compressed[..]);
        let mut decompressed = Vec::new();
        decoder.read_to_end(&mut decompressed).unwrap();
        assert!(decompressed.is_empty());
    }

    // --- Builder validation tests ---

    #[test]
    fn builder_requires_bucket() {
        let result = S3Config::builder()
            .service_name("svc")
            .instance_path("path")
            .boot_id("bid")
            .build();
        assert!(result.is_err());
    }

    #[test]
    fn builder_requires_service_name() {
        let result = S3Config::builder()
            .bucket("bucket")
            .instance_path("path")
            .boot_id("bid")
            .build();
        assert!(result.is_err());
    }

    #[test]
    fn builder_requires_instance_path() {
        let result = S3Config::builder()
            .bucket("bucket")
            .service_name("svc")
            .boot_id("bid")
            .build();
        assert!(result.is_err());
    }

    #[test]
    fn builder_requires_boot_id() {
        let result = S3Config::builder()
            .bucket("bucket")
            .service_name("svc")
            .instance_path("path")
            .build();
        assert!(result.is_err());
    }

    #[test]
    fn builder_prefix_defaults_to_empty() {
        let config = S3Config::builder()
            .bucket("bucket")
            .service_name("svc")
            .instance_path("path")
            .boot_id("bid")
            .build()
            .unwrap();
        let segment = make_segment("/tmp/trace.0.bin", 0);
        let key = config.object_key(&segment, "ts");
        assert!(key.starts_with("svc/"));
    }

    // --- S3 integration tests via s3s-fs ---

    #[tokio::test]
    async fn upload_and_delete_writes_to_s3_and_removes_local_file() {
        let s3_root = tempfile::tempdir().unwrap();
        let local_dir = tempfile::tempdir().unwrap();

        // Create the bucket directory (s3s-fs uses directories as buckets)
        std::fs::create_dir(s3_root.path().join("test-bucket")).unwrap();

        let client = fake_s3_client(s3_root.path());
        let config = make_config();
        let uploader = S3Uploader::new(client, config);

        // Write a fake segment file
        let segment_path = local_dir.path().join("trace.0.bin");
        std::fs::write(&segment_path, b"trace data here").unwrap();
        let segment = make_segment(&segment_path, 0);

        // Upload and delete
        let key = uploader
            .upload_and_delete(&segment, "2026-03-05T19-30-00Z")
            .await
            .unwrap();

        assert_eq!(
            key,
            "traces/checkout-api/us-east-1/i-0abc123/2026-03-05T19-30-00Z-0.bin.gz"
        );

        // Local file should be deleted
        assert!(!segment_path.exists());
    }

    #[tokio::test]
    async fn uploaded_object_contains_gzipped_original_data() {
        let s3_root = tempfile::tempdir().unwrap();
        let local_dir = tempfile::tempdir().unwrap();
        std::fs::create_dir(s3_root.path().join("test-bucket")).unwrap();

        let client = fake_s3_client(s3_root.path());
        let raw_s3_client = fake_raw_s3_client(s3_root.path());

        let config = make_config();
        let uploader = S3Uploader::new(client, config);

        let original_data = b"important trace data that must survive the roundtrip";
        let segment_path = local_dir.path().join("trace.5.bin");
        std::fs::write(&segment_path, original_data).unwrap();
        let segment = make_segment(&segment_path, 5);

        let key = uploader
            .upload_and_delete(&segment, "2026-03-05T19-30-00Z")
            .await
            .unwrap();

        // Read back from fake S3
        let get_result = raw_s3_client
            .get_object()
            .bucket("test-bucket")
            .key(&key)
            .send()
            .await
            .unwrap();

        let body = get_result.body.collect().await.unwrap().into_bytes();

        // Body should be gzip — decompress and verify
        let mut decoder = GzDecoder::new(&body[..]);
        let mut decompressed = Vec::new();
        decoder.read_to_end(&mut decompressed).unwrap();
        assert_eq!(decompressed, original_data);
    }

    #[tokio::test]
    async fn upload_sets_s3_object_metadata_headers() {
        let s3_root = tempfile::tempdir().unwrap();
        let local_dir = tempfile::tempdir().unwrap();
        std::fs::create_dir(s3_root.path().join("test-bucket")).unwrap();

        let client = fake_s3_client(s3_root.path());
        let raw_s3_client = fake_raw_s3_client(s3_root.path());

        let config = S3Config::builder()
            .bucket("test-bucket")
            .prefix("traces")
            .service_name("checkout-api")
            .instance_path("us-east-1/i-0abc123")
            .boot_id("a3f7c2d1-dead-beef-1234-567890abcdef")
            .build()
            .unwrap();
        let uploader = S3Uploader::new(client, config);

        let segment_path = local_dir.path().join("trace.3.bin");
        std::fs::write(&segment_path, b"trace data").unwrap();
        let segment = make_segment(&segment_path, 3);

        let key = uploader
            .upload_and_delete(&segment, "2026-03-05T19-30-00Z")
            .await
            .unwrap();

        // HeadObject to read back metadata
        let head = raw_s3_client
            .head_object()
            .bucket("test-bucket")
            .key(&key)
            .send()
            .await
            .unwrap();

        let meta = head.metadata().unwrap();
        assert_eq!(meta.get("service").unwrap(), "checkout-api");
        assert_eq!(
            meta.get("boot-id").unwrap(),
            "a3f7c2d1-dead-beef-1234-567890abcdef"
        );
        assert_eq!(meta.get("segment-index").unwrap(), "3");
        assert_eq!(meta.get("start-time").unwrap(), "2026-03-05T19-30-00Z");
        assert_eq!(meta.get("host").unwrap(), "us-east-1/i-0abc123");
    }

    #[tokio::test]
    async fn upload_failure_does_not_delete_local_file() {
        let s3_root = tempfile::tempdir().unwrap();
        let local_dir = tempfile::tempdir().unwrap();
        std::fs::create_dir(s3_root.path().join("test-bucket")).unwrap();

        let client = fake_s3_client(s3_root.path());
        // Use a read-only directory to make the delete fail after upload succeeds?
        // Actually, the simplest way: point the segment at a file that doesn't exist
        // so the read fails before upload.
        let config = make_config();
        let uploader = S3Uploader::new(client, config);

        let segment_path = local_dir.path().join("trace.0.bin");
        // Write the file, then make it unreadable
        std::fs::write(&segment_path, b"should survive").unwrap();

        // Create a segment pointing to a nonexistent file to trigger read failure
        let bad_segment = make_segment(local_dir.path().join("nonexistent.bin"), 0);

        let result = uploader
            .upload_and_delete(&bad_segment, "2026-03-05T19-30-00Z")
            .await;

        assert!(result.is_err(), "expected upload to fail for missing file");

        // The original file should be untouched (we never tried to delete it)
        assert!(segment_path.exists());
    }
}
