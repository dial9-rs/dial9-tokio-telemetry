mod axum_traced;
mod buffer;
mod ddb;
mod routes;

use std::sync::Arc;
use std::time::Duration;

use aws_config::BehaviorVersion;
use clap::Parser;
use dial9_tokio_telemetry::telemetry::{
    CpuProfilingConfig, RotatingWriter, SchedEventConfig, TracedRuntime,
};
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
}

#[derive(Clone)]
pub struct AppState {
    pub buffer: Arc<MetricsBuffer>,
    pub ddb: Arc<DdbClient>,
    /// Cancels the server's graceful-shutdown future. The client process
    /// triggers this indirectly via `POST /terminate`.
    pub shutdown: CancellationToken,
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
    let (runtime, guard) = TracedRuntime::builder()
        .with_task_tracking(true)
        .with_cpu_profiling(CpuProfilingConfig::default())
        .with_inline_callframe_symbols(true)
        .with_sched_events(SchedEventConfig {
            include_kernel: true,
        })
        .build(builder, Box::new(writer))?;
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

        let mut client_child = tokio::process::Command::new(&client_exe)
            .arg("--server-url")
            .arg(&server_url)
            .arg("--run-duration")
            .arg(args.run_duration.to_string())
            .spawn()
            .unwrap_or_else(|e| {
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
    });

    Ok(())
}
