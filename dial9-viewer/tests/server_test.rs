use assert2::check;
use dial9_viewer::server::{AppState, router};
use dial9_viewer::storage::{ObjectInfo, StorageBackend, StorageError};
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

#[tokio::test]
async fn serves_static_files() {
    let state = AppState {
        backend: Arc::new(FakeBackend),
        default_bucket: None,
        default_prefix: None,
    };
    let base = start_server(state).await;
    let client = reqwest::Client::new();

    // browser.html should be served
    let resp = client
        .get(format!("{base}/browser.html"))
        .send()
        .await
        .unwrap();
    check!(resp.status().as_u16() == 200);
    let body = resp.text().await.unwrap();
    check!(body.contains("dial9"));

    // index.html (the trace viewer) should be served
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
    let state = AppState {
        backend: Arc::new(FakeBackend),
        default_bucket: None,
        default_prefix: None,
    };
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
    let state = AppState {
        backend: Arc::new(FakeBackend),
        default_bucket: Some("my-bucket".into()),
        default_prefix: None,
    };
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
    let state = AppState {
        backend: Arc::new(FakeBackend),
        default_bucket: Some("b".into()),
        default_prefix: None,
    };
    let base = start_server(state).await;
    let client = reqwest::Client::new();

    let resp = client
        .get(format!("{base}/api/trace?keys=&bucket=b"))
        .send()
        .await
        .unwrap();
    check!(resp.status().as_u16() == 400);
}
