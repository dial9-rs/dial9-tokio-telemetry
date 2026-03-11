//! Stress test: can the S3 background worker keep up with a busy 64-worker runtime?
//!
//! Runs a high-throughput workload with small segment sizes to force rapid rotation,
//! then monitors the backlog of sealed-but-not-uploaded segments on disk.
//!
//! ```bash
//! cargo run --release -p dial9-tokio-telemetry --example s3_stress_test -- \
//!   --trace-path /tmp/stress/trace.bin --bucket my-bucket
//! ```
#![cfg(feature = "worker-s3")]

use clap::Parser;
use dial9_tokio_telemetry::background_task::BackgroundTaskConfig;
use dial9_tokio_telemetry::background_task::s3::S3Config;
use dial9_tokio_telemetry::telemetry::{RotatingWriter, TracedRuntime};
use metrique::local::{LocalFormat, OutputStyle};
use metrique::writer::format::FormatExt;
use metrique::writer::sink::FlushImmediatelyBuilder;
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{Duration, Instant};

#[derive(Parser)]
struct Args {
    #[arg(long)]
    trace_path: String,
    #[arg(long)]
    bucket: String,
    #[arg(long, default_value = "stress-test")]
    prefix: String,
    #[arg(long)]
    region: Option<String>,
    #[arg(long, default_value = "64")]
    worker_threads: usize,
    #[arg(long, default_value = "30", help = "Seconds to generate load")]
    duration: u64,
    #[arg(long, default_value = "1048576", help = "Bytes per segment")]
    segment_size: u64,
    #[arg(long, default_value = "104857600", help = "Max total disk (100MB)")]
    total_size: u64,
}

/// Count sealed .bin files (not .active) in the trace directory.
fn count_sealed_files(dir: &std::path::Path, stem: &str) -> u32 {
    let Ok(entries) = std::fs::read_dir(dir) else {
        return 0;
    };
    entries
        .filter_map(|e| e.ok())
        .filter(|e| {
            let name = e.file_name();
            let name = name.to_string_lossy();
            name.starts_with(stem) && name.ends_with(".bin") && !name.ends_with(".active")
        })
        .count() as u32
}

fn main() -> std::io::Result<()> {
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| "info,dial9_worker=info".parse().unwrap()),
        )
        .init();

    let args = Args::parse();
    let trace_dir = std::path::Path::new(&args.trace_path)
        .parent()
        .unwrap()
        .to_path_buf();
    let trace_stem = std::path::Path::new(&args.trace_path)
        .file_stem()
        .unwrap()
        .to_string_lossy()
        .to_string();

    std::fs::create_dir_all(&trace_dir)?;

    let writer = RotatingWriter::new(&args.trace_path, args.segment_size, args.total_size)?;

    let s3_config = S3Config::builder()
        .bucket(&args.bucket)
        .prefix(&args.prefix)
        .service_name("s3-stress-test")
        .instance_path(
            hostname::get()
                .unwrap_or_default()
                .to_string_lossy()
                .to_string(),
        )
        .boot_id(uuid::Uuid::new_v4().to_string())
        .maybe_region(args.region.as_ref())
        .build();

    let metrics_sink = FlushImmediatelyBuilder::new().build_boxed(
        LocalFormat::new(OutputStyle::Pretty).output_to_makewriter(|| std::io::stderr().lock()),
    );

    let uploader_config = BackgroundTaskConfig::builder()
        .trace_path(&args.trace_path)
        .s3(s3_config)
        .metrics_sink(metrics_sink)
        .build();

    let mut builder = tokio::runtime::Builder::new_multi_thread();
    builder.worker_threads(args.worker_threads).enable_all();

    let (runtime, guard) = TracedRuntime::builder()
        .with_task_tracking(true)
        .with_s3_uploader(uploader_config)
        .build_and_start(builder, writer)?;

    let handle = guard.handle();
    let load_duration = Duration::from_secs(args.duration);
    let tasks_done = Arc::new(AtomicU64::new(0));
    let start = Instant::now();

    eprintln!("=== S3 Worker Stress Test ===");
    eprintln!("  Workers:      {}", args.worker_threads);
    eprintln!("  Load duration: {}s", args.duration);
    eprintln!("  Segment size: {} bytes", args.segment_size);
    eprintln!();

    runtime.block_on(async {
        let counter = tasks_done.clone();
        let trace_dir2 = trace_dir.clone();
        let trace_stem2 = trace_stem.clone();

        // Spawn the workload
        let spawner = handle.spawn(async move {
            loop {
                if start.elapsed() >= load_duration {
                    break;
                }
                let mut joins = Vec::with_capacity(200);
                for _ in 0..200 {
                    let c = counter.clone();
                    joins.push(tokio::spawn(async move {
                        tokio::task::yield_now().await;
                        tokio::task::yield_now().await;
                        c.fetch_add(1, Ordering::Relaxed);
                    }));
                }
                for j in joins {
                    let _ = j.await;
                }
            }
        });

        // Monitor backlog every second
        let monitor = tokio::task::spawn_blocking(move || {
            let mut max_backlog = 0u32;
            loop {
                std::thread::sleep(Duration::from_secs(1));
                let elapsed = start.elapsed();
                let backlog = count_sealed_files(&trace_dir2, &trace_stem2);
                let tasks = tasks_done.load(Ordering::Relaxed);
                if backlog > max_backlog {
                    max_backlog = backlog;
                }
                let phase = if elapsed < load_duration {
                    "LOAD"
                } else {
                    "DRAIN"
                };
                eprintln!(
                    "  [{:>5.1}s] [{phase}] tasks: {tasks:>10}, backlog: {backlog:>3} sealed files (peak: {max_backlog})",
                    elapsed.as_secs_f64(),
                );
                // Stop monitoring once drain is complete
                if elapsed >= load_duration && backlog == 0 {
                    eprintln!();
                    let drain_time = elapsed - load_duration;
                    eprintln!("=== Results ===");
                    eprintln!("  Load phase:    {:.1}s", load_duration.as_secs_f64());
                    if drain_time.as_millis() > 100 {
                        eprintln!(
                            "  Drain phase:   {:.1}s (worker catching up after load stopped)",
                            drain_time.as_secs_f64()
                        );
                    } else {
                        eprintln!("  Drain phase:   worker kept up — no catch-up needed");
                    }
                    eprintln!("  Peak backlog:  {max_backlog} sealed files");
                    eprintln!("  Total tasks:   {tasks}");
                    break;
                }
                // Safety: don't monitor forever
                if elapsed > load_duration + Duration::from_secs(120) {
                    eprintln!("  ⚠ Timed out waiting for drain (backlog: {backlog})");
                    break;
                }
            }
        });

        let _ = spawner.await;
        eprintln!();
        eprintln!("Load complete, waiting for worker to drain...");

        // Wait for monitor to see backlog hit 0, then shut down
        let _ = monitor.await;

        eprintln!("Calling graceful_shutdown...");
        guard
            .graceful_shutdown(Duration::from_secs(30))
            .await
            .expect("graceful shutdown");
        eprintln!("Done.");
    });

    Ok(())
}
