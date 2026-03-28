//! Stress test: can the S3 background worker keep up with a busy 64-worker runtime?
//!
//! Runs a high-throughput workload with small segment sizes to force rapid rotation,
//! then monitors the backlog of sealed-but-not-uploaded segments on disk.
//! After shutdown, lists S3 to report how many segments were uploaded vs created.
//!
//! ```bash
//! cargo run --release -p dial9-tokio-telemetry --example s3_stress_test -- \
//!   --trace-path /tmp/stress/trace.bin --bucket my-bucket
//! ```
#![cfg(feature = "worker-s3")]

use clap::Parser;
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

/// List all S3 objects under a prefix. Returns (count, max_segment_index).
/// Segment index is parsed from the key suffix `{epoch}-{index}.bin.gz`.
async fn list_s3_objects(
    client: &aws_sdk_s3::Client,
    bucket: &str,
    prefix: &str,
) -> (u64, Option<u64>) {
    let mut count = 0u64;
    let mut max_index: Option<u64> = None;
    let mut continuation_token = None;
    loop {
        let mut req = client.list_objects_v2().bucket(bucket).prefix(prefix);
        if let Some(token) = continuation_token.take() {
            req = req.continuation_token(token);
        }
        match req.send().await {
            Ok(resp) => {
                for obj in resp.contents() {
                    count += 1;
                    // Parse segment index from key like ".../{epoch}-{index}.bin.gz"
                    if let Some(key) = obj.key()
                        && let Some(filename) = key.rsplit('/').next()
                    {
                        let stem = filename
                            .strip_suffix(".bin.gz")
                            .or_else(|| filename.strip_suffix(".bin"));
                        if let Some(stem) = stem
                            && let Some(idx_str) = stem.rsplit('-').next()
                            && let Ok(idx) = idx_str.parse::<u64>()
                        {
                            max_index = Some(max_index.map_or(idx, |cur: u64| cur.max(idx)));
                        }
                    }
                }
                if resp.is_truncated() == Some(true) {
                    continuation_token = resp.next_continuation_token().map(String::from);
                } else {
                    return (count, max_index);
                }
            }
            Err(e) => {
                eprintln!("  ⚠ Failed to list S3 objects: {e}");
                return (count, max_index);
            }
        }
    }
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

    let mut builder = tokio::runtime::Builder::new_multi_thread();
    builder.worker_threads(args.worker_threads).enable_all();

    let (runtime, guard) = TracedRuntime::builder()
        .with_task_tracking(true)
        .with_trace_path(&args.trace_path)
        .with_s3_uploader(s3_config)
        .with_worker_metrics_sink(metrics_sink)
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
                if elapsed > load_duration + Duration::from_secs(120) {
                    eprintln!("  ⚠ Timed out waiting for drain (backlog: {backlog})");
                    break;
                }
            }
        });

        let _ = spawner.await;
        eprintln!();
        eprintln!("Load complete, waiting for worker to drain...");
        let _ = monitor.await;
    });

    eprintln!("Calling graceful_shutdown...");
    drop(runtime);
    guard
        .graceful_shutdown(Duration::from_secs(30))
        .expect("graceful shutdown");
    eprintln!("Done.");

    // Count uploaded objects in S3
    eprintln!();
    eprintln!(
        "Counting S3 objects under s3://{}/{}/ ...",
        args.bucket, args.prefix
    );
    let count_rt = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()?;
    count_rt.block_on(async {
        let mut sdk_conf = aws_config::defaults(aws_config::BehaviorVersion::latest());
        if let Some(region) = &args.region {
            sdk_conf = sdk_conf.region(aws_config::Region::new(region.clone()));
        }
        let client = aws_sdk_s3::Client::new(&sdk_conf.load().await);
        let (uploaded, max_idx) = list_s3_objects(&client, &args.bucket, &args.prefix).await;

        // max segment index + 1 = total segments created (indices are 0-based)
        let total_created = max_idx.map(|i| i + 1).unwrap_or(uploaded);
        let missed = total_created.saturating_sub(uploaded);

        eprintln!();
        eprintln!("=== Upload Summary ===");
        eprintln!("  Total segments created: {total_created}");
        eprintln!("  Uploaded to S3:         {uploaded}");
        eprintln!("  Missed (evicted):       {missed}");
        if total_created > 0 {
            eprintln!(
                "  Upload rate:            {:.1}%",
                uploaded as f64 / total_created as f64 * 100.0
            );
        }
    });

    Ok(())
}
