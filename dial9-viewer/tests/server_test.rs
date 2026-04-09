use assert2::check;
use dial9_viewer::server::{AppState, router};
use dial9_viewer::storage::{ObjectInfo, S3Backend, StorageBackend, StorageError};
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
    let app = router(state, &ui_dir);
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
        .get(format!("{base}/browser.html"))
        .send()
        .await
        .unwrap();
    check!(resp.status().as_u16() == 200);
    let body = resp.text().await.unwrap();
    check!(body.contains("dial9"));

    let resp = client
        .get(format!("{base}/index.html"))
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
