use clap::Parser;
use std::path::PathBuf;

#[derive(Parser, Debug)]
#[command(
    name = "dial9-viewer",
    about = "S3 trace browser and viewer for dial9-tokio-telemetry"
)]
pub struct Cli {
    /// Port to listen on
    #[arg(long, default_value = "3000")]
    port: u16,

    /// S3 bucket name (if omitted, bucket must be specified per-request)
    #[arg(long)]
    bucket: Option<String>,

    /// S3 key prefix to filter traces
    #[arg(long)]
    prefix: Option<String>,

    /// Directory containing UI static files
    #[arg(long, default_value = "ui")]
    ui_dir: PathBuf,
}

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| "dial9_viewer=info".parse().unwrap()),
        )
        .init();

    let cli = Cli::parse();

    // Resolve ui_dir relative to the binary's location if it doesn't exist at CWD
    let ui_dir = if cli.ui_dir.exists() {
        cli.ui_dir.clone()
    } else if let Ok(exe) = std::env::current_exe() {
        let candidate = exe.parent().unwrap_or(exe.as_ref()).join(&cli.ui_dir);
        if candidate.exists() {
            candidate
        } else {
            cli.ui_dir.clone()
        }
    } else {
        cli.ui_dir.clone()
    };

    let backend = dial9_viewer::storage::S3Backend::from_env().await;
    let app_state = dial9_viewer::server::AppState::new(
        std::sync::Arc::new(backend),
        cli.bucket.clone(),
        cli.prefix.clone(),
    );

    let app = dial9_viewer::server::router(app_state, &ui_dir);

    let listener = tokio::net::TcpListener::bind(("0.0.0.0", cli.port)).await?;
    tracing::info!(port = cli.port, ui_dir = %ui_dir.display(), "dial9-viewer listening");
    if let Some(bucket) = &cli.bucket {
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
