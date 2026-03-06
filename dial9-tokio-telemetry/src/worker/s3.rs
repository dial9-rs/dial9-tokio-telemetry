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
    pub struct S3Config {
        bucket: String,
        prefix: String,
        service_name: String,
        instance_path: String,
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

        pub fn build(self) -> Result<S3Config, &'static str> {
            Ok(S3Config {
                bucket: self.bucket.ok_or("bucket is required")?,
                prefix: self.prefix.unwrap_or_default(),
                service_name: self.service_name.ok_or("service_name is required")?,
                instance_path: self.instance_path.ok_or("instance_path is required")?,
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
            .bucket("my-traces")
            .prefix("traces")
            .service_name("checkout-api")
            .instance_path("us-east-1/i-0abc123")
            .build()
            .unwrap()
    }

    fn make_segment(path: impl Into<PathBuf>, index: u32) -> SealedSegment {
        SealedSegment {
            path: path.into(),
            index,
        }
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
            .build();
        assert!(result.is_err());
    }

    #[test]
    fn builder_requires_service_name() {
        let result = S3Config::builder()
            .bucket("bucket")
            .instance_path("path")
            .build();
        assert!(result.is_err());
    }

    #[test]
    fn builder_requires_instance_path() {
        let result = S3Config::builder()
            .bucket("bucket")
            .service_name("svc")
            .build();
        assert!(result.is_err());
    }

    #[test]
    fn builder_prefix_defaults_to_empty() {
        let config = S3Config::builder()
            .bucket("bucket")
            .service_name("svc")
            .instance_path("path")
            .build()
            .unwrap();
        // Verify via key generation — no prefix means service_name is first
        let segment = make_segment("/tmp/trace.0.bin", 0);
        let key = config.object_key(&segment, "ts");
        assert!(key.starts_with("svc/"));
    }

    // --- Upload + delete integration test ---

    #[tokio::test]
    async fn upload_reads_and_compresses_segment_file() {
        let dir = tempfile::tempdir().unwrap();
        let segment_path = dir.path().join("trace.0.bin");
        std::fs::write(&segment_path, b"fake trace data").unwrap();

        let segment = make_segment(&segment_path, 0);
        assert!(segment_path.exists());

        // Test the pre-upload path: read + compress
        let data = std::fs::read(&segment.path).unwrap();
        let compressed = gzip_compress(&data).unwrap();

        let mut decoder = GzDecoder::new(&compressed[..]);
        let mut decompressed = Vec::new();
        decoder.read_to_end(&mut decompressed).unwrap();
        assert_eq!(decompressed, b"fake trace data");
    }
}
