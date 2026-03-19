mod axum_traced;
mod buffer;
mod ddb;
mod routes;

use std::sync::Arc;
use std::time::Duration;

use aws_config::BehaviorVersion;
use clap::Parser;
#[cfg(target_os = "linux")]
use dial9_tokio_telemetry::telemetry::{CpuProfilingConfig, SchedEventConfig};
use dial9_tokio_telemetry::telemetry::{RotatingWriter, TracedRuntime};
use tokio::runtime::Builder;
use tokio_util::sync::CancellationToken;

use buffer::MetricsBuffer;
use ddb::DdbClient;

#[derive(Parser)]
#[command(about = "Metrics service with DynamoDB persistence and telemetry")]
struct Args {
    #[arg(long, default_value = "1", help = "Flush interval in seconds")]
    flush_interval: u64,

    #[arg(long, default_value = "metrics-service", help = "DynamoDB table name")]
    table_name: String,

    #[arg(long, default_value = "0.0.0.0:3001", help = "Server bind address")]
    server_addr: String,

    #[arg(
        long,
        default_value = "55",
        help = "Run duration in seconds (passed to client)"
    )]
    run_duration: u64,

    #[arg(
        long,
        default_value = "/tmp/metrics-service-traces/trace.bin",
        help = "Trace file path"
    )]
    trace_path: String,

    #[arg(
        long,
        default_value = "100000000",
        help = "Max trace file size in bytes"
    )]
    trace_max_file_size: u64,

    #[arg(
        long,
        default_value = "314572800",
        help = "Max total trace size in bytes"
    )]
    trace_max_total_size: u64,

    #[arg(long, default_value = "4", help = "Number of worker threads")]
    worker_threads: usize,

    #[arg(long, help = "Demo mode: shorter run with smaller trace (<2MB)")]
    demo: bool,

    #[arg(
        long,
        help = "S3 bucket for trace upload (enables background S3 uploader)"
    )]
    s3_bucket: Option<String>,

    #[arg(long, help = "AWS region for S3 uploads (defaults to SDK default)")]
    s3_region: Option<String>,
}

#[derive(Clone)]
pub struct AppState {
    pub buffer: Arc<MetricsBuffer>,
    pub ddb: Arc<DdbClient>,
    /// Cancels the server's graceful-shutdown future. The client process
    /// triggers this indirectly via `POST /terminate`.
    pub shutdown: CancellationToken,
}

/// Pre-warm the kernel FD table to avoid RCU-synchronization stalls when the
/// table grows under load.  See <https://github.com/tokio-rs/tokio/issues/7970>.
///
/// Opens `/dev/null`, then uses `fcntl(fd, F_DUPFD_CLOEXEC, target)` to force
/// the kernel to expand the table to at least `target` entries.  Both FDs are
/// closed immediately; the table capacity persists for the process lifetime.
#[cfg(target_os = "linux")]
fn prewarm_fd_table(target: libc::c_int) {
    unsafe {
        let src = libc::open(c"/dev/null".as_ptr(), libc::O_RDONLY | libc::O_CLOEXEC);
        if src < 0 {
            tracing::warn!("fd prewarm: failed to open /dev/null");
            return;
        }
        let dup = libc::fcntl(src, libc::F_DUPFD_CLOEXEC, target);
        if dup < 0 {
            tracing::warn!(target, "fd prewarm: fcntl F_DUPFD_CLOEXEC failed");
        } else {
            tracing::info!(target, actual = dup, "fd table pre-warmed");
            libc::close(dup);
        }
        libc::close(src);
    }
}

fn main() -> std::io::Result<()> {
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| "info,dial9_worker=debug".parse().unwrap()),
        )
        .init();

    #[cfg(target_os = "linux")]
    if let Ok(val) = std::env::var("PREWARM_FD_TABLE_SIZE") {
        if let Ok(n) = val.parse::<libc::c_int>() {
            prewarm_fd_table(n);
        } else {
            tracing::warn!(val, "PREWARM_FD_TABLE_SIZE is not a valid integer");
        }
    }

    let mut args = Args::parse();

    if args.demo {
        args.run_duration = 4;
        args.worker_threads = 2;
        args.trace_max_file_size = 5_000_000;
        args.trace_max_total_size = 5_000_000;
    }

    let writer = RotatingWriter::new(
        &args.trace_path,
        args.trace_max_file_size,
        args.trace_max_total_size,
    )?;

    let mut builder = Builder::new_multi_thread();
    builder.worker_threads(args.worker_threads).enable_all();
    #[allow(unused_mut)]
    let mut traced_builder = TracedRuntime::builder()
        .with_trace_path(&args.trace_path)
        .with_task_tracking(true);
    #[cfg(target_os = "linux")]
    {
        traced_builder = traced_builder
            .with_cpu_profiling(CpuProfilingConfig::default())
            .with_sched_events(SchedEventConfig {
                include_kernel: true,
            });
    }
    if let Some(bucket) = &args.s3_bucket {
        use dial9_tokio_telemetry::background_task::s3::S3Config;

        let s3_config = S3Config::builder()
            .bucket(bucket)
            .prefix("traces")
            .service_name("metrics-service")
            .instance_path(
                hostname::get()
                    .unwrap_or_default()
                    .to_string_lossy()
                    .to_string(),
            )
            .boot_id(uuid::Uuid::new_v4().to_string())
            .maybe_region(args.s3_region.as_ref())
            .build();

        traced_builder = traced_builder.with_s3_uploader(s3_config);
    }
    let (runtime, guard) = traced_builder.build(builder, writer)?;
    guard.enable();
    let handle = guard.handle();

    runtime.block_on(async {
        let config = aws_config::defaults(BehaviorVersion::latest()).load().await;

        let shutdown = CancellationToken::new();

        let state = AppState {
            buffer: Arc::new(MetricsBuffer::new()),
            ddb: Arc::new(DdbClient::new(&config, &args.table_name)),
            shutdown: shutdown.clone(),
        };

        state
            .ddb
            .ensure_table()
            .await
            .expect("failed to ensure DynamoDB table");

        // background flush worker
        let flush_state = state.clone();
        let flush_interval = Duration::from_secs(args.flush_interval);
        handle.spawn(async move {
            let mut interval = tokio::time::interval(flush_interval);
            loop {
                interval.tick().await;
                flush_state.buffer.flush_to_ddb(&flush_state.ddb).await;
            }
        });

        let app = routes::router(state);
        let listener = tokio::net::TcpListener::bind(&args.server_addr)
            .await
            .unwrap();
        println!("Listening on http://{}", args.server_addr);

        // Spawn the client as a separate process. It owns the run-duration
        // timer and signals shutdown by calling `POST /terminate` when done.
        let port = args.server_addr.split(':').nth(1).unwrap_or("3001");
        let server_url = format!("http://127.0.0.1:{port}");
        let client_exe = std::env::current_exe()
            .expect("cannot determine current executable path")
            .parent()
            .expect("executable has no parent directory")
            .join("client");

        let mut client_cmd = tokio::process::Command::new(&client_exe);
        client_cmd
            .arg("--server-url")
            .arg(&server_url)
            .arg("--run-duration")
            .arg(args.run_duration.to_string());

        if args.demo {
            client_cmd.arg("--demo");
        }

        let mut client_child = client_cmd.spawn().unwrap_or_else(|e| {
            panic!(
                "failed to spawn client binary at {}: {e}",
                client_exe.display()
            )
        });

        // Reap the child when it exits so it doesn't become a zombie.
        handle.spawn(async move {
            match client_child.wait().await {
                Ok(status) => println!("Client process exited: {status}"),
                Err(e) => eprintln!("Error waiting for client process: {e}"),
            }
        });

        axum_traced::serve(listener, app.into_make_service(), handle.clone())
            .with_graceful_shutdown(async move { shutdown.cancelled().await })
            .await
            .unwrap();
        let shutdown = guard.graceful_shutdown(Duration::from_secs(5)).await;
        tracing::info!("dial9 shutdown: {shutdown:?}");
    });

    Ok(())
}
