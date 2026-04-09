/// Dev helper: starts an s3s fake S3 server, seeds it with test trace data,
/// then starts the dial9-viewer pointed at it.
///
/// Usage: cargo run -p dial9-viewer --bin dev-server
use std::io::Write;

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    tracing_subscriber::fmt()
        .with_env_filter("dial9_viewer=info,dev_server=info")
        .init();

    // Set up s3s-fs backed fake S3
    let s3_root = tempfile::tempdir()?;
    let bucket = "demo-traces";
    std::fs::create_dir(s3_root.path().join(bucket))?;

    let fs = s3s_fs::FileSystem::new(s3_root.path()).map_err(|e| anyhow::anyhow!("{e:?}"))?;
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

    let client = aws_sdk_s3::Client::from_conf(s3_config);

    // Seed with demo trace data — use the actual demo-trace.bin if available
    let demo_trace_path =
        std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("ui/demo-trace.bin");

    if demo_trace_path.exists() {
        let compressed = std::fs::read(&demo_trace_path)?;
        // demo-trace.bin is gzipped — decompress before splitting
        let demo_data = gunzip_bytes(&compressed);

        // Upload the full trace as a single gzipped segment
        let full_compressed = gzip_bytes(&demo_data);
        client
            .put_object()
            .bucket(bucket)
            .key("traces/2026-04-09/1900/demo-service/local/host-0/1744224000-0.bin.gz")
            .body(full_compressed.into())
            .send()
            .await?;
        tracing::info!(
            key = "traces/2026-04-09/1900/demo-service/local/host-0/1744224000-0.bin.gz",
            size = demo_data.len(),
            "seeded full demo trace"
        );
    } else {
        tracing::warn!("demo-trace.bin not found, seeding with synthetic data");
        for i in 0..5 {
            let data = format!("synthetic trace segment {i}");
            let compressed = gzip_bytes(data.as_bytes());
            let key =
                format!("traces/2026-04-09/191{i}/test-svc/us-east-1/host-1/1744224{i}00-0.bin.gz");
            client
                .put_object()
                .bucket(bucket)
                .key(&key)
                .body(compressed.into())
                .send()
                .await?;
            tracing::info!(%key, "seeded");
        }
    }

    // Start the viewer with the s3s-backed S3Backend
    let backend = dial9_viewer::storage::S3Backend::from_client(client);
    let state = dial9_viewer::server::AppState::new(
        std::sync::Arc::new(backend),
        Some(bucket.to_string()),
        Some("traces".to_string()),
    );

    let ui_dir = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("ui");
    let app = dial9_viewer::server::router(state, &ui_dir);

    let port: u16 = std::env::var("PORT")
        .ok()
        .and_then(|p| p.parse().ok())
        .unwrap_or(3000);
    let listener = tokio::net::TcpListener::bind(("0.0.0.0", port)).await?;
    tracing::info!("dial9-viewer dev server listening on http://localhost:{port}");
    tracing::info!("bucket={bucket}, prefix=traces");
    tracing::info!("try: http://localhost:{port}/browser.html");
    tracing::info!("search for: 2026-04-09/");

    axum::serve(listener, app)
        .with_graceful_shutdown(async {
            tokio::signal::ctrl_c().await.ok();
        })
        .await?;

    Ok(())
}

fn gzip_bytes(data: &[u8]) -> Vec<u8> {
    use flate2::Compression;
    use flate2::write::GzEncoder;
    let mut encoder = GzEncoder::new(Vec::new(), Compression::fast());
    encoder.write_all(data).unwrap();
    encoder.finish().unwrap()
}

fn gunzip_bytes(data: &[u8]) -> Vec<u8> {
    use flate2::read::GzDecoder;
    use std::io::Read;
    let mut decoder = GzDecoder::new(data);
    let mut out = Vec::new();
    decoder.read_to_end(&mut out).unwrap();
    out
}
