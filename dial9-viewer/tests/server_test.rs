use assert2::check;
use dial9_viewer::server::{AppState, router};
use dial9_viewer::storage::{LocalBackend, ObjectInfo, S3Backend, StorageBackend, StorageError};
use std::future::Future;
use std::pin::Pin;
use std::sync::Arc;

/// In-memory fake backend for tests that don't need S3.
struct FakeBackend;

impl StorageBackend for FakeBackend {
    fn list_objects(
        &self,
        _bucket: &str,
        _prefix: &str,
    ) -> Pin<Box<dyn Future<Output = Result<Vec<ObjectInfo>, StorageError>> + Send + '_>> {
        Box::pin(async { Ok(vec![]) })
    }

    fn list_prefixes(
        &self,
        _bucket: &str,
        _prefix: &str,
    ) -> Pin<Box<dyn Future<Output = Result<Vec<String>, StorageError>> + Send + '_>> {
        Box::pin(async { Ok(vec![]) })
    }

    fn get_object(
        &self,
        _bucket: &str,
        _key: &str,
    ) -> Pin<Box<dyn Future<Output = Result<Vec<u8>, StorageError>> + Send + '_>> {
        Box::pin(async { Err(StorageError::NotFound("fake".into())) })
    }
}

fn fake_s3_client(fs_root: &std::path::Path) -> aws_sdk_s3::Client {
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

async fn start_server(state: AppState) -> String {
    let ui_dir = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("ui");
    let state = state.with_dev_ui_dir(ui_dir);
    let app = router(state);
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    tokio::spawn(async move {
        axum::serve(listener, app).await.unwrap();
    });
    format!("http://{addr}")
}

// --- basic tests ---

#[tokio::test]
async fn serves_static_files() {
    let state = AppState::new(Arc::new(FakeBackend), None, None);
    let base = start_server(state).await;
    let client = reqwest::Client::new();

    let resp = client
        .get(format!("{base}/index.html"))
        .send()
        .await
        .unwrap();
    check!(resp.status().as_u16() == 200);
    let body = resp.text().await.unwrap();
    check!(body.contains("dial9"));

    let resp = client
        .get(format!("{base}/viewer.html"))
        .send()
        .await
        .unwrap();
    check!(resp.status().as_u16() == 200);
    let body = resp.text().await.unwrap();
    check!(body.contains("Trace Viewer"));
}

#[tokio::test]
async fn search_requires_bucket() {
    let state = AppState::new(Arc::new(FakeBackend), None, None);
    let base = start_server(state).await;
    let client = reqwest::Client::new();

    let resp = client
        .get(format!("{base}/api/search"))
        .send()
        .await
        .unwrap();
    check!(resp.status().as_u16() == 400);
}

#[tokio::test]
async fn search_uses_default_bucket() {
    let state = AppState::new(Arc::new(FakeBackend), Some("my-bucket".into()), None);
    let base = start_server(state).await;
    let client = reqwest::Client::new();

    let resp = client
        .get(format!("{base}/api/search"))
        .send()
        .await
        .unwrap();
    check!(resp.status().as_u16() == 200);
    let body: Vec<ObjectInfo> = resp.json().await.unwrap();
    check!(body.is_empty());
}

#[tokio::test]
async fn trace_requires_keys() {
    let state = AppState::new(Arc::new(FakeBackend), Some("test-bucket".into()), None);
    let base = start_server(state).await;
    let client = reqwest::Client::new();

    let resp = client
        .get(format!("{base}/api/trace?keys=&bucket=test-bucket"))
        .send()
        .await
        .unwrap();
    check!(resp.status().as_u16() == 400);
}

#[tokio::test]
async fn config_returns_defaults() {
    let state = AppState::new(
        Arc::new(FakeBackend),
        Some("my-bucket".into()),
        Some("my-prefix".into()),
    );
    let base = start_server(state).await;
    let client = reqwest::Client::new();

    let resp: serde_json::Value = client
        .get(format!("{base}/api/config"))
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    check!(resp["default_bucket"] == "my-bucket");
    check!(resp["default_prefix"] == "my-prefix");
}

#[tokio::test]
async fn config_returns_nulls_when_no_defaults() {
    let state = AppState::new(Arc::new(FakeBackend), None, None);
    let base = start_server(state).await;
    let client = reqwest::Client::new();

    let resp: serde_json::Value = client
        .get(format!("{base}/api/config"))
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    check!(resp["default_bucket"].is_null());
    check!(resp["default_prefix"].is_null());
}

// --- s3s integration tests ---

/// Upload a test object via the S3 API.
async fn put_object(client: &aws_sdk_s3::Client, bucket: &str, key: &str, data: &[u8]) {
    client
        .put_object()
        .bucket(bucket)
        .key(key)
        .body(data.to_vec().into())
        .send()
        .await
        .unwrap();
}

fn gzip_bytes(data: &[u8]) -> Vec<u8> {
    use flate2::Compression;
    use flate2::write::GzEncoder;
    use std::io::Write;
    let mut encoder = GzEncoder::new(Vec::new(), Compression::fast());
    encoder.write_all(data).unwrap();
    encoder.finish().unwrap()
}

/// Set up a fake S3 environment: returns (upload_client, backend, base_url).
async fn setup_s3_test(
    bucket: &str,
    default_bucket: Option<String>,
    default_prefix: Option<String>,
) -> (aws_sdk_s3::Client, String, tempfile::TempDir) {
    let s3_root = tempfile::tempdir().unwrap();
    std::fs::create_dir(s3_root.path().join(bucket)).unwrap();

    // One client for uploading test data
    let upload_client = fake_s3_client(s3_root.path());
    // Separate client for the backend (s3s-fs doesn't share state across instances,
    // but they share the same filesystem root)
    let backend = S3Backend::from_client(fake_s3_client(s3_root.path()));

    let state = AppState::new(Arc::new(backend), default_bucket, default_prefix);
    let base = start_server(state).await;
    (upload_client, base, s3_root)
}

#[tokio::test]
async fn prefixes_discovers_top_level_prefixes() {
    let (s3, base, _dir) = setup_s3_test("test-bucket", Some("test-bucket".into()), None).await;
    let client = reqwest::Client::new();

    put_object(&s3, "test-bucket", "traces/2026-04-09/file.bin", b"data").await;
    put_object(&s3, "test-bucket", "logs/other.bin", b"data").await;

    let resp: Vec<String> = client
        .get(format!("{base}/api/prefixes"))
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    check!(resp.contains(&"traces/".to_string()));
    check!(resp.contains(&"logs/".to_string()));
}

#[tokio::test]
async fn search_returns_objects_from_s3() {
    let (s3, base, _dir) = setup_s3_test("test-bucket", None, None).await;
    let client = reqwest::Client::new();

    put_object(
        &s3,
        "test-bucket",
        "traces/2026-04-09/1910/svc/host/123-0.bin.gz",
        &gzip_bytes(b"trace data 1"),
    )
    .await;
    put_object(
        &s3,
        "test-bucket",
        "traces/2026-04-09/1910/svc/host/123-1.bin.gz",
        &gzip_bytes(b"trace data 2"),
    )
    .await;
    put_object(
        &s3,
        "test-bucket",
        "traces/2026-04-09/1920/svc/host/456-0.bin.gz",
        &gzip_bytes(b"other data"),
    )
    .await;

    // Search with prefix matching the 1910 time bucket
    let resp = client
        .get(format!(
            "{base}/api/search?q=traces/2026-04-09/1910/&bucket=test-bucket"
        ))
        .send()
        .await
        .unwrap();
    check!(resp.status().as_u16() == 200);
    let body: Vec<ObjectInfo> = resp.json().await.unwrap();
    check!(body.len() == 2);
    check!(body[0].key.contains("1910"));
    check!(body[1].key.contains("1910"));

    // Search with broader prefix returns all 3
    let resp = client
        .get(format!(
            "{base}/api/search?q=traces/2026-04-09/&bucket=test-bucket"
        ))
        .send()
        .await
        .unwrap();
    let body: Vec<ObjectInfo> = resp.json().await.unwrap();
    check!(body.len() == 3);
}

#[tokio::test]
async fn search_with_default_prefix() {
    let (s3, base, _dir) = setup_s3_test(
        "test-bucket",
        Some("test-bucket".into()),
        Some("my-prefix".into()),
    )
    .await;
    let client = reqwest::Client::new();

    put_object(
        &s3,
        "test-bucket",
        "my-prefix/2026-04-09/1910/svc/host/123-0.bin.gz",
        b"data",
    )
    .await;

    let resp = client
        .get(format!("{base}/api/search?q=2026-04-09/"))
        .send()
        .await
        .unwrap();
    check!(resp.status().as_u16() == 200);
    let body: Vec<ObjectInfo> = resp.json().await.unwrap();
    check!(body.len() == 1);
    check!(body[0].key.contains("my-prefix"));
}

#[tokio::test]
async fn trace_fetches_and_concatenates() {
    let (s3, base, _dir) = setup_s3_test("test-bucket", Some("test-bucket".into()), None).await;
    let client = reqwest::Client::new();

    let trace1 = b"TRACE_SEGMENT_1_DATA";
    let trace2 = b"TRACE_SEGMENT_2_DATA";

    put_object(&s3, "test-bucket", "seg1.bin.gz", &gzip_bytes(trace1)).await;
    put_object(&s3, "test-bucket", "seg2.bin.gz", &gzip_bytes(trace2)).await;

    let resp = client
        .get(format!(
            "{base}/api/trace?keys=seg1.bin.gz&keys=seg2.bin.gz"
        ))
        .send()
        .await
        .unwrap();
    check!(resp.status().as_u16() == 200);

    let body = resp.bytes().await.unwrap();
    let expected: Vec<u8> = [trace1.as_slice(), trace2.as_slice()].concat();
    check!(body.as_ref() == expected.as_slice());
}

#[tokio::test]
async fn trace_single_key() {
    let (s3, base, _dir) = setup_s3_test("trace-bucket", Some("trace-bucket".into()), None).await;
    let client = reqwest::Client::new();

    let data = b"single segment data";
    put_object(&s3, "trace-bucket", "key.bin.gz", &gzip_bytes(data)).await;

    let resp = client
        .get(format!("{base}/api/trace?keys=key.bin.gz"))
        .send()
        .await
        .unwrap();
    check!(resp.status().as_u16() == 200);
    let body = resp.bytes().await.unwrap();
    check!(body.as_ref() == data);
}

#[tokio::test]
async fn trace_handles_uncompressed_data() {
    let (s3, base, _dir) = setup_s3_test("trace-bucket", Some("trace-bucket".into()), None).await;
    let client = reqwest::Client::new();

    let data = b"raw uncompressed trace";
    put_object(&s3, "trace-bucket", "raw.bin", data).await;

    let resp = client
        .get(format!("{base}/api/trace?keys=raw.bin"))
        .send()
        .await
        .unwrap();
    check!(resp.status().as_u16() == 200);
    let body = resp.bytes().await.unwrap();
    check!(body.as_ref() == data);
}

/// Full end-to-end smoke test: simulates the browser flow.
/// 1. Upload gzipped trace segments to fake S3
/// 2. Search for them via /api/search
/// 3. Pick keys from the search results
/// 4. Fetch concatenated trace via /api/trace
/// 5. Verify the concatenated output matches the original data
#[tokio::test]
async fn e2e_search_then_view() {
    let (s3, base, _dir) = setup_s3_test("traces-bucket", Some("traces-bucket".into()), None).await;
    let client = reqwest::Client::new();

    // Simulate two trace segments from the same time bucket
    let seg1 = b"SEGMENT_ONE_BINARY_DATA_HERE";
    let seg2 = b"SEGMENT_TWO_BINARY_DATA_HERE";

    put_object(
        &s3,
        "traces-bucket",
        "2026-04-09/1910/checkout-api/us-east-1/host1/1000-0.bin.gz",
        &gzip_bytes(seg1),
    )
    .await;
    put_object(
        &s3,
        "traces-bucket",
        "2026-04-09/1910/checkout-api/us-east-1/host1/1000-1.bin.gz",
        &gzip_bytes(seg2),
    )
    .await;

    // Step 1: Search — like the browser would
    let search_resp: Vec<ObjectInfo> = client
        .get(format!(
            "{base}/api/search?q=2026-04-09/1910/checkout-api/&bucket=traces-bucket"
        ))
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();

    check!(search_resp.len() == 2);

    // Step 2: Build the trace URL from search results — like the browser's viewSelected()
    let keys: Vec<&str> = search_resp.iter().map(|o| o.key.as_str()).collect();
    let keys_param: String = keys
        .iter()
        .map(|k| format!("keys={}", urlencoding::encode(k)))
        .collect::<Vec<_>>()
        .join("&");

    let trace_resp = client
        .get(format!(
            "{base}/api/trace?{keys_param}&bucket=traces-bucket",
        ))
        .send()
        .await
        .unwrap();

    check!(trace_resp.status().as_u16() == 200);
    let body = trace_resp.bytes().await.unwrap();

    // The concatenated output should be seg1 + seg2 (order depends on S3 listing)
    check!(body.len() == seg1.len() + seg2.len());
    // Both segments should be present
    let body_slice = body.as_ref();
    let has_seg1 = body_slice.windows(seg1.len()).any(|w| w == seg1.as_slice());
    let has_seg2 = body_slice.windows(seg2.len()).any(|w| w == seg2.as_slice());
    check!(has_seg1);
    check!(has_seg2);
}

/// Regression test: a compressed segment that decompresses to >50MB must be
/// served successfully. Previously, the server truncated at 50MB during
/// decompression (overshooting by up to 8KB) and then rejected the result
/// with HTTP 413 because it exceeded the same 50MB limit.
#[tokio::test]
async fn trace_serves_large_decompressed_segment() {
    let (s3, base, _dir) = setup_s3_test("big-bucket", Some("big-bucket".into()), None).await;
    let client = reqwest::Client::new();

    // 60MB of data — compresses well, decompresses to >50MB
    let big_data = vec![0xABu8; 60 * 1024 * 1024];
    put_object(&s3, "big-bucket", "big.bin.gz", &gzip_bytes(&big_data)).await;

    let resp = client
        .get(format!("{base}/api/trace?keys=big.bin.gz"))
        .send()
        .await
        .unwrap();
    check!(resp.status().as_u16() == 200);
    let body = resp.bytes().await.unwrap();
    check!(body.len() == big_data.len());
}

// --- local backend tests ---

fn setup_local_dir() -> tempfile::TempDir {
    let dir = tempfile::tempdir().unwrap();
    // Create a directory structure mimicking trace output:
    //   2026-04-09/1910/svc/host/123-0.bin.gz
    //   2026-04-09/1910/svc/host/123-1.bin.gz
    //   2026-04-09/1920/svc/host/456-0.bin.gz
    let base = dir.path().join("2026-04-09/1910/svc/host");
    std::fs::create_dir_all(&base).unwrap();
    std::fs::write(base.join("123-0.bin.gz"), gzip_bytes(b"trace seg 0")).unwrap();
    std::fs::write(base.join("123-1.bin.gz"), gzip_bytes(b"trace seg 1")).unwrap();

    let base2 = dir.path().join("2026-04-09/1920/svc/host");
    std::fs::create_dir_all(&base2).unwrap();
    std::fs::write(base2.join("456-0.bin.gz"), gzip_bytes(b"other trace")).unwrap();
    dir
}

fn local_state(dir: &std::path::Path) -> AppState {
    AppState::new(Arc::new(LocalBackend::new(dir)), Some("local".into()), None)
}

#[tokio::test]
async fn local_search_lists_all_files() {
    let dir = setup_local_dir();
    let base = start_server(local_state(dir.path())).await;
    let client = reqwest::Client::new();

    let resp: Vec<ObjectInfo> = client
        .get(format!("{base}/api/search"))
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    check!(resp.len() == 3);
}

#[tokio::test]
async fn local_search_filters_by_prefix() {
    let dir = setup_local_dir();
    let base = start_server(local_state(dir.path())).await;
    let client = reqwest::Client::new();

    let resp: Vec<ObjectInfo> = client
        .get(format!("{base}/api/search?q=2026-04-09/1910/"))
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    check!(resp.len() == 2);
    for obj in &resp {
        check!(obj.key.contains("1910"));
    }
}

#[tokio::test]
async fn local_trace_fetches_and_decompresses() {
    let dir = setup_local_dir();
    let base = start_server(local_state(dir.path())).await;
    let client = reqwest::Client::new();

    let resp = client
        .get(format!(
            "{base}/api/trace?keys=2026-04-09/1910/svc/host/123-0.bin.gz&keys=2026-04-09/1910/svc/host/123-1.bin.gz"
        ))
        .send()
        .await
        .unwrap();
    check!(resp.status().as_u16() == 200);
    let body = resp.bytes().await.unwrap();
    // Both segments decompressed and concatenated
    let body_slice = body.as_ref();
    let has_seg0 = body_slice
        .windows(b"trace seg 0".len())
        .any(|w| w == b"trace seg 0");
    let has_seg1 = body_slice
        .windows(b"trace seg 1".len())
        .any(|w| w == b"trace seg 1");
    check!(has_seg0);
    check!(has_seg1);
}

#[tokio::test]
async fn local_trace_not_found() {
    let dir = setup_local_dir();
    let base = start_server(local_state(dir.path())).await;
    let client = reqwest::Client::new();

    let resp = client
        .get(format!("{base}/api/trace?keys=nonexistent.bin"))
        .send()
        .await
        .unwrap();
    check!(resp.status().as_u16() == 500);
}

#[tokio::test]
async fn local_prefixes_lists_subdirs() {
    let dir = setup_local_dir();
    let base = start_server(local_state(dir.path())).await;
    let client = reqwest::Client::new();

    // Top-level prefixes
    let resp: Vec<String> = client
        .get(format!("{base}/api/prefixes"))
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    check!(resp == vec!["2026-04-09/"]);

    // Nested prefixes
    let resp: Vec<String> = client
        .get(format!("{base}/api/prefixes?prefix=2026-04-09/"))
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    check!(resp.contains(&"2026-04-09/1910/".to_string()));
    check!(resp.contains(&"2026-04-09/1920/".to_string()));
}

#[tokio::test]
async fn local_e2e_search_then_view() {
    let dir = setup_local_dir();
    let base = start_server(local_state(dir.path())).await;
    let client = reqwest::Client::new();

    // Search for segments in the 1910 time bucket
    let search_resp: Vec<ObjectInfo> = client
        .get(format!("{base}/api/search?q=2026-04-09/1910/"))
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    check!(search_resp.len() == 2);

    // Build trace URL from search results
    let keys_param: String = search_resp
        .iter()
        .map(|o| format!("keys={}", urlencoding::encode(&o.key)))
        .collect::<Vec<_>>()
        .join("&");

    let trace_resp = client
        .get(format!("{base}/api/trace?{keys_param}"))
        .send()
        .await
        .unwrap();
    check!(trace_resp.status().as_u16() == 200);

    let body = trace_resp.bytes().await.unwrap();
    // Both segments present (decompressed)
    let body_slice = body.as_ref();
    let has_seg0 = body_slice
        .windows(b"trace seg 0".len())
        .any(|w| w == b"trace seg 0");
    let has_seg1 = body_slice
        .windows(b"trace seg 1".len())
        .any(|w| w == b"trace seg 1");
    check!(has_seg0);
    check!(has_seg1);
}

#[tokio::test]
async fn local_search_empty_dir() {
    let dir = tempfile::tempdir().unwrap();
    let base = start_server(local_state(dir.path())).await;
    let client = reqwest::Client::new();

    let resp: Vec<ObjectInfo> = client
        .get(format!("{base}/api/search"))
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    check!(resp.is_empty());
}

#[tokio::test]
async fn local_search_returns_file_sizes() {
    let dir = tempfile::tempdir().unwrap();
    let data = b"hello world";
    std::fs::write(dir.path().join("test.bin"), data).unwrap();

    let base = start_server(local_state(dir.path())).await;
    let client = reqwest::Client::new();

    let resp: Vec<ObjectInfo> = client
        .get(format!("{base}/api/search"))
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    check!(resp.len() == 1);
    check!(resp[0].key == "test.bin");
    check!(resp[0].size == data.len() as i64);
}

#[tokio::test]
async fn local_path_traversal_rejected() {
    let dir = setup_local_dir();
    let base = start_server(local_state(dir.path())).await;
    let client = reqwest::Client::new();

    // Attempt to escape root via ../
    let resp = client
        .get(format!("{base}/api/trace?keys=../../../etc/passwd"))
        .send()
        .await
        .unwrap();
    // Should fail — either not found or error, but not 200
    check!(resp.status().as_u16() != 200);
}

#[cfg(test)]
mod skills_unpack_tests {
    use std::path::Path;
    use std::process::Command;

    fn validate_skill_frontmatter(skill_dir: &Path, content: &str) {
        assert!(
            content.starts_with("---\n"),
            "SKILL.md in {skill_dir:?} missing frontmatter delimiter"
        );

        // Validate name field matches directory name
        let dir_name = skill_dir.file_name().unwrap().to_string_lossy().to_string();
        let name_line = content
            .lines()
            .find(|l| l.starts_with("name:"))
            .unwrap_or_else(|| panic!("SKILL.md in {skill_dir:?} missing name field"));
        let name = name_line.strip_prefix("name:").unwrap().trim();
        assert_eq!(
            name, dir_name,
            "name field {:?} doesn't match directory {:?}",
            name, dir_name
        );

        // Validate name format: lowercase + hyphens, no leading/trailing/consecutive hyphens
        assert!(
            !name.is_empty() && name.len() <= 64,
            "name {:?} invalid length",
            name
        );
        assert!(
            name.chars()
                .all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == '-'),
            "name {:?} contains invalid characters",
            name
        );
        assert!(
            !name.starts_with('-') && !name.ends_with('-'),
            "name {:?} has leading/trailing hyphen",
            name
        );
        assert!(
            !name.contains("--"),
            "name {:?} has consecutive hyphens",
            name
        );

        // Validate description field exists and is a non-empty scalar string.
        let desc_line = content
            .lines()
            .find(|l| l.starts_with("description:"))
            .unwrap_or_else(|| panic!("SKILL.md in {skill_dir:?} missing description field"));
        let desc = desc_line.strip_prefix("description:").unwrap().trim();
        assert!(!desc.is_empty(), "empty description in {skill_dir:?}");
        assert!(
            !(desc.starts_with('[') || desc.starts_with('{')),
            "description must be a scalar string in {skill_dir:?}: {desc:?}"
        );
        assert!(
            desc.len() <= 1024,
            "description too long ({}) in {:?}",
            desc.len(),
            skill_dir
        );
    }

    /// Validates source skill definitions in `dial9-viewer/skills/` have valid frontmatter.
    #[test]
    fn source_skills_have_valid_frontmatter() {
        let skills_dir = Path::new(env!("CARGO_MANIFEST_DIR")).join("skills");
        let mut skill_count = 0;

        for entry in std::fs::read_dir(&skills_dir).unwrap() {
            let entry = entry.unwrap();
            if !entry.path().is_dir() {
                continue;
            }
            skill_count += 1;
            let skill_md = entry.path().join("SKILL.md");
            assert!(skill_md.exists(), "Missing SKILL.md in {:?}", entry.path());

            let content = std::fs::read_to_string(&skill_md).unwrap();
            validate_skill_frontmatter(&entry.path(), &content);
        }

        assert!(
            skill_count >= 6,
            "expected at least 6 source skills, got {skill_count}"
        );
    }

    /// Validates that `agents skills <dir>` produces a valid Agent Skills directory.
    /// Each skill must have a SKILL.md with valid frontmatter (name + description).
    #[test]
    fn unpack_produces_valid_agent_skills_layout() {
        let bin = env!("CARGO_BIN_EXE_dial9-viewer");
        let dir = tempfile::tempdir().unwrap();
        let output = Command::new(bin)
            .args(["agents", "skills", dir.path().to_str().unwrap()])
            .output()
            .unwrap();
        assert!(
            output.status.success(),
            "agents skills failed: {:?}",
            output
        );

        // Each subdirectory must have a SKILL.md with valid frontmatter
        let mut skill_count = 0;
        for entry in std::fs::read_dir(dir.path()).unwrap() {
            let entry = entry.unwrap();
            if !entry.path().is_dir() {
                continue;
            }
            skill_count += 1;
            let skill_md = entry.path().join("SKILL.md");
            assert!(skill_md.exists(), "Missing SKILL.md in {:?}", entry.path());

            let content = std::fs::read_to_string(&skill_md).unwrap();
            validate_skill_frontmatter(&entry.path(), &content);
        }
        assert!(
            skill_count >= 6,
            "expected at least 6 skills, got {skill_count}"
        );
    }

    /// Validates that `agents toolkit <dir>` produces the expected JS files.
    #[test]
    fn toolkit_produces_expected_files() {
        let bin = env!("CARGO_BIN_EXE_dial9-viewer");
        let dir = tempfile::tempdir().unwrap();
        let output = Command::new(bin)
            .args(["agents", "toolkit", dir.path().to_str().unwrap()])
            .output()
            .unwrap();
        assert!(
            output.status.success(),
            "agents toolkit failed: {:?}",
            output
        );

        let expected = [
            "analyze.js",
            "decode.js",
            "trace_parser.js",
            "trace_analysis.js",
        ];
        for name in &expected {
            let path = dir.path().join(name);
            assert!(path.exists(), "missing toolkit file: {name}");
            assert!(
                std::fs::metadata(&path).unwrap().len() > 0,
                "empty toolkit file: {name}"
            );
        }
    }

    /// Validates that scripts in unpacked skills can find their dependencies.
    /// The red_flag_scan.js should be able to resolve trace_parser.js from the toolkit skill.
    #[test]
    fn unpacked_scripts_resolve_dependencies() {
        let bin = env!("CARGO_BIN_EXE_dial9-viewer");
        let dir = tempfile::tempdir().unwrap();
        let output = Command::new(bin)
            .args(["agents", "skills", dir.path().to_str().unwrap()])
            .output()
            .unwrap();
        assert!(output.status.success());

        // The toolkit skill must have all 4 scripts
        let toolkit_scripts = dir.path().join("dial9-toolkit").join("scripts");
        assert!(toolkit_scripts.join("analyze.js").exists());
        assert!(toolkit_scripts.join("decode.js").exists());
        assert!(toolkit_scripts.join("trace_parser.js").exists());
        assert!(toolkit_scripts.join("trace_analysis.js").exists());

        // The red-flags skill must have its script
        let red_flags_script = dir
            .path()
            .join("dial9-red-flags")
            .join("scripts")
            .join("red_flag_scan.js");
        assert!(red_flags_script.exists());
    }
}
