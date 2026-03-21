//! Fake S3 backends for integration tests.
//!
//! Provides in-process S3 clients backed by `s3s-fs` with optional
//! region enforcement, flaky injection, and hanging behavior.

/// Create an `aws_sdk_s3::Client` backed by s3s-fs (in-memory fake S3).
pub fn fake_s3_client(fs_root: &std::path::Path) -> aws_sdk_s3::Client {
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

/// s3s wrapper that enforces a specific bucket region.
/// `head_bucket` returns the expected region. All other operations reject
/// requests whose `region` field doesn't match, simulating S3's 301 redirect.
struct RegionEnforcingFs<S> {
    inner: S,
    expected_region: String,
}

impl<S> RegionEnforcingFs<S> {
    fn check_region<T>(&self, req: &s3s::S3Request<T>) -> s3s::S3Result<()> {
        match &req.region {
            Some(r) if r.as_str() == self.expected_region => Ok(()),
            other => Err(s3s::S3Error::with_message(
                s3s::S3ErrorCode::PermanentRedirect,
                format!(
                    "wrong region: got {:?}, expected {}",
                    other, self.expected_region
                ),
            )),
        }
    }
}

#[async_trait::async_trait]
impl<S: s3s::S3 + Send + Sync> s3s::S3 for RegionEnforcingFs<S> {
    async fn head_bucket(
        &self,
        _req: s3s::S3Request<s3s::dto::HeadBucketInput>,
    ) -> s3s::S3Result<s3s::S3Response<s3s::dto::HeadBucketOutput>> {
        let output = s3s::dto::HeadBucketOutput {
            bucket_region: Some(self.expected_region.clone()),
            ..Default::default()
        };
        Ok(s3s::S3Response::new(output))
    }

    async fn put_object(
        &self,
        req: s3s::S3Request<s3s::dto::PutObjectInput>,
    ) -> s3s::S3Result<s3s::S3Response<s3s::dto::PutObjectOutput>> {
        self.check_region(&req)?;
        self.inner.put_object(req).await
    }

    async fn get_object(
        &self,
        req: s3s::S3Request<s3s::dto::GetObjectInput>,
    ) -> s3s::S3Result<s3s::S3Response<s3s::dto::GetObjectOutput>> {
        self.check_region(&req)?;
        self.inner.get_object(req).await
    }

    async fn list_objects_v2(
        &self,
        req: s3s::S3Request<s3s::dto::ListObjectsV2Input>,
    ) -> s3s::S3Result<s3s::S3Response<s3s::dto::ListObjectsV2Output>> {
        self.check_region(&req)?;
        self.inner.list_objects_v2(req).await
    }

    async fn create_multipart_upload(
        &self,
        req: s3s::S3Request<s3s::dto::CreateMultipartUploadInput>,
    ) -> s3s::S3Result<s3s::S3Response<s3s::dto::CreateMultipartUploadOutput>> {
        self.check_region(&req)?;
        self.inner.create_multipart_upload(req).await
    }

    async fn upload_part(
        &self,
        req: s3s::S3Request<s3s::dto::UploadPartInput>,
    ) -> s3s::S3Result<s3s::S3Response<s3s::dto::UploadPartOutput>> {
        self.check_region(&req)?;
        self.inner.upload_part(req).await
    }

    async fn complete_multipart_upload(
        &self,
        req: s3s::S3Request<s3s::dto::CompleteMultipartUploadInput>,
    ) -> s3s::S3Result<s3s::S3Response<s3s::dto::CompleteMultipartUploadOutput>> {
        self.check_region(&req)?;
        self.inner.complete_multipart_upload(req).await
    }
}

/// Build an `aws_sdk_s3::Client` backed by `RegionEnforcingFs`.
/// The client is intentionally configured with the WRONG region (`us-west-2`).
/// Only requests corrected to `expected_region` will succeed.
pub fn fake_s3_client_with_region(
    fs_root: &std::path::Path,
    expected_region: &str,
) -> aws_sdk_s3::Client {
    let fs = s3s_fs::FileSystem::new(fs_root).unwrap();
    let region_fs = RegionEnforcingFs {
        inner: fs,
        expected_region: expected_region.to_owned(),
    };
    let mut builder = s3s::service::S3ServiceBuilder::new(region_fs);
    builder.set_auth(s3s::auth::SimpleAuth::from_single("test", "test"));
    let s3_service = builder.build();
    let s3_client: s3s_aws::Client = s3_service.into();

    let s3_config = aws_sdk_s3::Config::builder()
        .behavior_version_latest()
        .credentials_provider(aws_sdk_s3::config::Credentials::new(
            "test", "test", None, None, "test",
        ))
        .region(aws_sdk_s3::config::Region::new("us-west-2"))
        .http_client(s3_client)
        .force_path_style(true)
        .build();

    aws_sdk_s3::Client::from_conf(s3_config)
}

/// s3s wrapper that fails `put_object` and `upload_part` calls at a configurable rate.
struct FlakyS3<S> {
    inner: S,
    fail_counter: std::sync::atomic::AtomicU64,
    fail_every_n: u64,
}

impl<S> FlakyS3<S> {
    fn maybe_fail(&self) -> s3s::S3Result<()> {
        let n = self
            .fail_counter
            .fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        if n.is_multiple_of(self.fail_every_n) {
            return Err(s3s::S3Error::with_message(
                s3s::S3ErrorCode::InternalError,
                "injected failure",
            ));
        }
        Ok(())
    }
}

#[async_trait::async_trait]
impl<S: s3s::S3 + Send + Sync> s3s::S3 for FlakyS3<S> {
    async fn head_bucket(
        &self,
        req: s3s::S3Request<s3s::dto::HeadBucketInput>,
    ) -> s3s::S3Result<s3s::S3Response<s3s::dto::HeadBucketOutput>> {
        self.inner.head_bucket(req).await
    }

    async fn put_object(
        &self,
        req: s3s::S3Request<s3s::dto::PutObjectInput>,
    ) -> s3s::S3Result<s3s::S3Response<s3s::dto::PutObjectOutput>> {
        self.maybe_fail()?;
        self.inner.put_object(req).await
    }

    async fn get_object(
        &self,
        req: s3s::S3Request<s3s::dto::GetObjectInput>,
    ) -> s3s::S3Result<s3s::S3Response<s3s::dto::GetObjectOutput>> {
        self.inner.get_object(req).await
    }

    async fn list_objects_v2(
        &self,
        req: s3s::S3Request<s3s::dto::ListObjectsV2Input>,
    ) -> s3s::S3Result<s3s::S3Response<s3s::dto::ListObjectsV2Output>> {
        self.inner.list_objects_v2(req).await
    }

    async fn create_multipart_upload(
        &self,
        req: s3s::S3Request<s3s::dto::CreateMultipartUploadInput>,
    ) -> s3s::S3Result<s3s::S3Response<s3s::dto::CreateMultipartUploadOutput>> {
        self.inner.create_multipart_upload(req).await
    }

    async fn upload_part(
        &self,
        req: s3s::S3Request<s3s::dto::UploadPartInput>,
    ) -> s3s::S3Result<s3s::S3Response<s3s::dto::UploadPartOutput>> {
        self.maybe_fail()?;
        self.inner.upload_part(req).await
    }

    async fn complete_multipart_upload(
        &self,
        req: s3s::S3Request<s3s::dto::CompleteMultipartUploadInput>,
    ) -> s3s::S3Result<s3s::S3Response<s3s::dto::CompleteMultipartUploadOutput>> {
        self.inner.complete_multipart_upload(req).await
    }
}

/// Build an `aws_sdk_s3::Client` that enforces region AND fails every Nth `put_object`.
pub fn fake_s3_client_flaky(
    fs_root: &std::path::Path,
    expected_region: &str,
    fail_every_n: u64,
) -> aws_sdk_s3::Client {
    let fs = s3s_fs::FileSystem::new(fs_root).unwrap();
    let region_fs = RegionEnforcingFs {
        inner: fs,
        expected_region: expected_region.to_owned(),
    };
    let flaky = FlakyS3 {
        inner: region_fs,
        fail_counter: std::sync::atomic::AtomicU64::new(0),
        fail_every_n,
    };
    let mut builder = s3s::service::S3ServiceBuilder::new(flaky);
    builder.set_auth(s3s::auth::SimpleAuth::from_single("test", "test"));
    let s3_service = builder.build();
    let s3_client: s3s_aws::Client = s3_service.into();

    let s3_config = aws_sdk_s3::Config::builder()
        .behavior_version_latest()
        .credentials_provider(aws_sdk_s3::config::Credentials::new(
            "test", "test", None, None, "test",
        ))
        .region(aws_sdk_s3::config::Region::new("us-west-2"))
        .http_client(s3_client)
        .force_path_style(true)
        .build();

    aws_sdk_s3::Client::from_conf(s3_config)
}

/// s3s wrapper where `put_object` hangs forever (the future never resolves).
struct HangingS3<S> {
    inner: S,
}

#[async_trait::async_trait]
impl<S: s3s::S3 + Send + Sync> s3s::S3 for HangingS3<S> {
    async fn head_bucket(
        &self,
        req: s3s::S3Request<s3s::dto::HeadBucketInput>,
    ) -> s3s::S3Result<s3s::S3Response<s3s::dto::HeadBucketOutput>> {
        self.inner.head_bucket(req).await
    }

    async fn put_object(
        &self,
        _req: s3s::S3Request<s3s::dto::PutObjectInput>,
    ) -> s3s::S3Result<s3s::S3Response<s3s::dto::PutObjectOutput>> {
        std::future::pending().await
    }

    async fn get_object(
        &self,
        req: s3s::S3Request<s3s::dto::GetObjectInput>,
    ) -> s3s::S3Result<s3s::S3Response<s3s::dto::GetObjectOutput>> {
        self.inner.get_object(req).await
    }

    async fn list_objects_v2(
        &self,
        req: s3s::S3Request<s3s::dto::ListObjectsV2Input>,
    ) -> s3s::S3Result<s3s::S3Response<s3s::dto::ListObjectsV2Output>> {
        self.inner.list_objects_v2(req).await
    }

    async fn create_multipart_upload(
        &self,
        req: s3s::S3Request<s3s::dto::CreateMultipartUploadInput>,
    ) -> s3s::S3Result<s3s::S3Response<s3s::dto::CreateMultipartUploadOutput>> {
        self.inner.create_multipart_upload(req).await
    }

    async fn upload_part(
        &self,
        req: s3s::S3Request<s3s::dto::UploadPartInput>,
    ) -> s3s::S3Result<s3s::S3Response<s3s::dto::UploadPartOutput>> {
        self.inner.upload_part(req).await
    }

    async fn complete_multipart_upload(
        &self,
        req: s3s::S3Request<s3s::dto::CompleteMultipartUploadInput>,
    ) -> s3s::S3Result<s3s::S3Response<s3s::dto::CompleteMultipartUploadOutput>> {
        self.inner.complete_multipart_upload(req).await
    }
}

/// Build an `aws_sdk_s3::Client` where `put_object` hangs forever.
pub fn fake_s3_client_hanging(fs_root: &std::path::Path) -> aws_sdk_s3::Client {
    let fs = s3s_fs::FileSystem::new(fs_root).unwrap();
    let hanging = HangingS3 { inner: fs };
    let mut builder = s3s::service::S3ServiceBuilder::new(hanging);
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

/// s3s wrapper where every `put_object` and `upload_part` returns 500.
struct AlwaysFailS3<S> {
    inner: S,
}

#[async_trait::async_trait]
impl<S: s3s::S3 + Send + Sync> s3s::S3 for AlwaysFailS3<S> {
    async fn head_bucket(
        &self,
        req: s3s::S3Request<s3s::dto::HeadBucketInput>,
    ) -> s3s::S3Result<s3s::S3Response<s3s::dto::HeadBucketOutput>> {
        self.inner.head_bucket(req).await
    }

    async fn put_object(
        &self,
        _req: s3s::S3Request<s3s::dto::PutObjectInput>,
    ) -> s3s::S3Result<s3s::S3Response<s3s::dto::PutObjectOutput>> {
        Err(s3s::S3Error::with_message(
            s3s::S3ErrorCode::InternalError,
            "permanent failure",
        ))
    }

    async fn get_object(
        &self,
        req: s3s::S3Request<s3s::dto::GetObjectInput>,
    ) -> s3s::S3Result<s3s::S3Response<s3s::dto::GetObjectOutput>> {
        self.inner.get_object(req).await
    }

    async fn list_objects_v2(
        &self,
        req: s3s::S3Request<s3s::dto::ListObjectsV2Input>,
    ) -> s3s::S3Result<s3s::S3Response<s3s::dto::ListObjectsV2Output>> {
        self.inner.list_objects_v2(req).await
    }

    async fn create_multipart_upload(
        &self,
        _req: s3s::S3Request<s3s::dto::CreateMultipartUploadInput>,
    ) -> s3s::S3Result<s3s::S3Response<s3s::dto::CreateMultipartUploadOutput>> {
        Err(s3s::S3Error::with_message(
            s3s::S3ErrorCode::InternalError,
            "permanent failure",
        ))
    }

    async fn upload_part(
        &self,
        _req: s3s::S3Request<s3s::dto::UploadPartInput>,
    ) -> s3s::S3Result<s3s::S3Response<s3s::dto::UploadPartOutput>> {
        Err(s3s::S3Error::with_message(
            s3s::S3ErrorCode::InternalError,
            "permanent failure",
        ))
    }

    async fn complete_multipart_upload(
        &self,
        _req: s3s::S3Request<s3s::dto::CompleteMultipartUploadInput>,
    ) -> s3s::S3Result<s3s::S3Response<s3s::dto::CompleteMultipartUploadOutput>> {
        Err(s3s::S3Error::with_message(
            s3s::S3ErrorCode::InternalError,
            "permanent failure",
        ))
    }
}

/// Build an `aws_sdk_s3::Client` where all uploads permanently fail with 500.
pub fn fake_s3_client_always_failing(fs_root: &std::path::Path) -> aws_sdk_s3::Client {
    let fs = s3s_fs::FileSystem::new(fs_root).unwrap();
    let failing = AlwaysFailS3 { inner: fs };
    let mut builder = s3s::service::S3ServiceBuilder::new(failing);
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
