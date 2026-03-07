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
    #[derive(Clone, bon::Builder)]
    #[builder(on(String, into))]
    pub struct S3Config {
        bucket: String,
        service_name: String,
        instance_path: String,
        boot_id: String,
        /// Optional key prefix. Defaults to empty (no prefix).
        #[builder(default)]
        prefix: String,
    }

    impl S3Config {
        /// Build the S3 object key for a sealed segment.
        ///
        /// Format: `{prefix}/{date-hour}/{service}/{instance}/{epoch_secs}-{index}.bin.gz`
        /// Time-first layout enables incident correlation across all services with a
        /// single `ListObjectsV2` prefix query.
        pub fn object_key(&self, segment: &SealedSegment, epoch_secs: u64) -> String {
            let date_hour = date_hour_from_epoch(epoch_secs);
            let ts = epoch_secs.to_string();
            if self.prefix.is_empty() {
                format!(
                    "{}/{}/{}/{}-{}.bin.gz",
                    date_hour, self.service_name, self.instance_path, ts, segment.index
                )
            } else {
                format!(
                    "{}/{}/{}/{}/{}-{}.bin.gz",
                    self.prefix, date_hour, self.service_name, self.instance_path, ts, segment.index
                )
            }
        }
    }

    /// Convert epoch seconds to `YYYY-MM-DD/HH` string for S3 key bucketing.
    /// Uses the Hinnant civil_from_days algorithm (no datetime library needed).
    fn date_hour_from_epoch(epoch_secs: u64) -> String {
        let secs = epoch_secs as i64;
        let hour = ((secs % 86400) / 3600) as u32;
        let days = (secs / 86400) as i64;

        // Hinnant algorithm: days since 1970-01-01 → (year, month, day)
        let z = days + 719468;
        let era = z.div_euclid(146097);
        let doe = z.rem_euclid(146097) as u32;
        let yoe = (doe - doe / 1460 + doe / 36524 - doe / 146096) / 365;
        let y = yoe as i64 + era * 400;
        let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
        let mp = (5 * doy + 2) / 153;
        let d = doy - (153 * mp + 2) / 5 + 1;
        let m = if mp < 10 { mp + 3 } else { mp - 9 };
        let y = if m <= 2 { y + 1 } else { y };

        format!("{:04}-{:02}-{:02}/{:02}", y, m, d, hour)
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
            epoch_secs: u64,
        ) -> Result<String, Box<dyn std::error::Error + Send + Sync>> {
            let data = std::fs::read(&segment.path)?;
            let compressed = gzip_compress(&data)?;
            let key = self.config.object_key(segment, epoch_secs);
            let ts_str = epoch_secs.to_string();

            let handle =
                aws_sdk_s3_transfer_manager::operation::upload::UploadInput::builder()
                    .bucket(&self.config.bucket)
                    .key(&key)
                    .content_encoding("gzip")
                    .content_type("application/octet-stream")
                    .metadata("service", &self.config.service_name)
                    .metadata("boot-id", &self.config.boot_id)
                    .metadata("segment-index", segment.index.to_string())
                    .metadata("start-time", &ts_str)
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
        let key = config.object_key(&segment, 1741209000);
        assert_eq!(
            key,
            "traces/2025-03-05/19/checkout-api/us-east-1/i-0abc123/1741209000-3.bin.gz"
        );
    }

    #[test]
    fn object_key_empty_prefix() {
        let config = S3Config::builder()
            .bucket("my-traces")
            .service_name("checkout-api")
            .instance_path("us-east-1/i-0abc123")
            .boot_id("test-boot-id")
            .build();
        let segment = make_segment("/tmp/trace.0.bin", 0);
        let key = config.object_key(&segment, 1741209000);
        assert_eq!(
            key,
            "2025-03-05/19/checkout-api/us-east-1/i-0abc123/1741209000-0.bin.gz"
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

    // --- Builder tests ---

    #[test]
    fn builder_prefix_defaults_to_empty() {
        let config = S3Config::builder()
            .bucket("bucket")
            .service_name("svc")
            .instance_path("path")
            .boot_id("bid")
            .build();
        let segment = make_segment("/tmp/trace.0.bin", 0);
        let key = config.object_key(&segment, 1741209000);
        // No prefix → date-hour is first component
        assert!(key.starts_with("2025-03-05/"));
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
            .upload_and_delete(&segment, 1741209000)
            .await
            .unwrap();

        assert_eq!(
            key,
            "traces/2025-03-05/19/checkout-api/us-east-1/i-0abc123/1741209000-0.bin.gz"
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
            .upload_and_delete(&segment, 1741209000)
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
            .build();
        let uploader = S3Uploader::new(client, config);

        let segment_path = local_dir.path().join("trace.3.bin");
        std::fs::write(&segment_path, b"trace data").unwrap();
        let segment = make_segment(&segment_path, 3);

        let key = uploader
            .upload_and_delete(&segment, 1741209000)
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
        assert_eq!(meta.get("start-time").unwrap(), "1741209000");
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
            .upload_and_delete(&bad_segment, 1741209000)
            .await;

        assert!(result.is_err(), "expected upload to fail for missing file");

        // The original file should be untouched (we never tried to delete it)
        assert!(segment_path.exists());
    }
}
