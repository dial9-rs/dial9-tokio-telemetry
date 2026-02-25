mod axum_traced;
mod buffer;
mod client;
mod ddb;
mod routes;

use std::sync::Arc;
use std::time::Duration;

use aws_config::BehaviorVersion;
use clap::Parser;
use dial9_tokio_telemetry::telemetry::{CpuProfilingConfig, RotatingWriter, TracedRuntime};
use tokio::runtime::Builder;

use buffer::MetricsBuffer;
use ddb::DdbClient;

#[derive(Parser)]
#[command(about = "Metrics service with DynamoDB persistence and telemetry")]
struct Args {
    #[arg(long, default_value = "10", help = "Flush interval in seconds")]
    flush_interval: u64,

    #[arg(long, default_value = "metrics-service", help = "DynamoDB table name")]
    table_name: String,

    #[arg(long, default_value = "0.0.0.0:3001", help = "Server bind address")]
    server_addr: String,

    #[arg(long, default_value = "55", help = "Run duration in seconds")]
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
}

#[derive(Clone)]
pub struct AppState {
    buffer: Arc<MetricsBuffer>,
    ddb: Arc<DdbClient>,
}

fn main() -> std::io::Result<()> {
    let args = Args::parse();

    let writer = RotatingWriter::new(
        &args.trace_path,
        args.trace_max_file_size,
        args.trace_max_total_size,
    )?;

    let mut builder = Builder::new_multi_thread();
    builder.worker_threads(args.worker_threads).enable_all();
    let (runtime, _guard) = TracedRuntime::builder()
        .with_task_tracking(true)
        .with_cpu_profiling(CpuProfilingConfig::default())
        .with_inline_callframe_symbols(true)
        .build_and_start(builder, Box::new(writer))?;
    let handle = _guard.handle();

    runtime.block_on(async {
        let config = aws_config::defaults(BehaviorVersion::latest()).load().await;
        let state = AppState {
            buffer: Arc::new(MetricsBuffer::new()),
            ddb: Arc::new(DdbClient::new(&config, &args.table_name)),
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

        let shutdown = tokio_util::sync::CancellationToken::new();

        // client harness
        let client_shutdown = shutdown.clone();
        let server_url = format!(
            "http://127.0.0.1:{}",
            args.server_addr.split(':').nth(1).unwrap_or("3001")
        );
        handle.spawn(async move {
            tokio::time::sleep(Duration::from_millis(100)).await;
            println!("Client starting load profile...");
            client::run(&server_url, client_shutdown.clone()).await;
        });

        // timer to stop everything
        let timer_shutdown = shutdown.clone();
        let run_duration = Duration::from_secs(args.run_duration);
        handle.spawn(async move {
            tokio::time::sleep(run_duration).await;
            println!("Run complete, shutting down.");
            timer_shutdown.cancel();
        });

        axum_traced::serve(listener, app.into_make_service(), handle.clone())
            .with_graceful_shutdown(async move { shutdown.cancelled().await })
            .await
            .unwrap();
    });

    Ok(())
}
