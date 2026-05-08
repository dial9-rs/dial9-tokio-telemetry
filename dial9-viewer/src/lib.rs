pub mod cli;
pub mod server;
pub mod storage;

use std::path::PathBuf;

async fn detect_bucket_region(bucket: &str) -> Option<String> {
    let config = aws_config::load_defaults(aws_config::BehaviorVersion::latest()).await;
    let client = aws_sdk_s3::Client::new(&config);
    match client.head_bucket().bucket(bucket).send().await {
        Ok(resp) => resp.bucket_region().map(|r| r.to_string()),
        Err(err) => {
            let raw = err.raw_response();
            raw.and_then(|r| {
                r.headers()
                    .get("x-amz-bucket-region")
                    .map(|v| v.to_string())
            })
        }
    }
}

pub(crate) async fn serve(
    port: u16,
    bucket: Option<String>,
    prefix: Option<String>,
    local_dir: Option<PathBuf>,
    dev: bool,
) -> anyhow::Result<()> {
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| "dial9_viewer=info".parse().unwrap()),
        )
        .init();

    let dev_ui_dir = if dev {
        let candidates = [PathBuf::from("ui"), PathBuf::from("dial9-viewer/ui")];
        let dir = candidates.into_iter().find(|p| p.exists());
        match dir {
            Some(d) => {
                tracing::info!(path = %d.display(), "dev mode: serving UI from disk");
                Some(d)
            }
            None => {
                anyhow::bail!(
                    "--dev: could not find ui/ directory. Run from the dial9-viewer/ or repo root directory."
                );
            }
        }
    } else {
        None
    };

    let app_state = if let Some(dir) = &local_dir {
        let dir = std::fs::canonicalize(dir)?;
        tracing::info!(path = %dir.display(), "serving traces from local directory");
        let backend = storage::LocalBackend::new(&dir);
        let mut state = server::AppState::new(
            std::sync::Arc::new(backend),
            Some("local".into()),
            prefix.clone(),
        );
        if let Some(d) = dev_ui_dir {
            state = state.with_dev_ui_dir(d);
        }
        state
    } else if let Some(bucket_name) = &bucket {
        if let Some(region) = detect_bucket_region(bucket_name).await {
            tracing::info!(%region, bucket = %bucket_name, "detected bucket region");
            let config = aws_config::defaults(aws_config::BehaviorVersion::latest())
                .region(aws_sdk_s3::config::Region::new(region))
                .load()
                .await;
            let client = aws_sdk_s3::Client::new(&config);
            let backend = storage::S3Backend::from_client(client);
            let mut state =
                server::AppState::new(std::sync::Arc::new(backend), bucket.clone(), prefix.clone());
            if let Some(d) = dev_ui_dir {
                state = state.with_dev_ui_dir(d);
            }
            state
        } else {
            tracing::warn!(bucket = %bucket_name, "could not detect bucket region, using default");
            let backend = storage::S3Backend::from_env().await;
            let mut state =
                server::AppState::new(std::sync::Arc::new(backend), bucket.clone(), prefix.clone());
            if let Some(d) = dev_ui_dir {
                state = state.with_dev_ui_dir(d);
            }
            state
        }
    } else {
        let backend = storage::S3Backend::from_env().await;
        let mut state =
            server::AppState::new(std::sync::Arc::new(backend), bucket.clone(), prefix.clone());
        if let Some(d) = dev_ui_dir {
            state = state.with_dev_ui_dir(d);
        }
        state
    };

    let app = server::router(app_state);

    let listener = tokio::net::TcpListener::bind(("0.0.0.0", port)).await?;
    tracing::info!(port, dev, "dial9-viewer listening");
    println!("\n  → http://localhost:{}\n", port);
    if let Some(dir) = &local_dir {
        tracing::info!(path = %dir.display(), "local directory mode");
    } else if let Some(bucket) = &bucket {
        tracing::info!(%bucket, "default bucket");
    }

    axum::serve(listener, app)
        .with_graceful_shutdown(shutdown_signal())
        .await?;

    Ok(())
}

async fn shutdown_signal() {
    tokio::signal::ctrl_c()
        .await
        .expect("failed to install CTRL+C handler");
    tracing::info!("shutting down");
}
