//! Realistic telemetry overhead benchmark.
//!
//! The server runs on a traced runtime. The load generator runs on a separate
//! plain runtime so it doesn't pollute the trace or compete for workers.
//!
//! Usage:
//!   cargo bench --bench overhead_bench -- <mode> [duration_secs]
//!
//! Modes:
//!   baseline  – plain tokio runtime, no hooks
//!   telemetry – hooks installed, writing to a temp file
//!   noop      – hooks installed, NullWriter (measures pure hook overhead)
//!
//! Duration defaults to 30 seconds. A 3-second warmup precedes measurement.

use dial9_tokio_telemetry::telemetry::{
    NullWriter, SimpleBinaryWriter, TelemetryGuard, TracedRuntime,
};
use hdrhistogram::Histogram;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::{Duration, Instant};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpListener;

const NUM_CLIENTS: usize = 50;
const DEFAULT_DURATION_SECS: u64 = 30;
const WARMUP_SECS: u64 = 3;

// ── Echo server (runs on the traced runtime) ─────────────────────────────────

async fn echo_server(listener: TcpListener) {
    loop {
        let (mut sock, _) = match listener.accept().await {
            Ok(c) => c,
            Err(_) => break,
        };
        tokio::spawn(async move {
            let mut buf = [0u8; 256];
            loop {
                let n = match sock.read(&mut buf).await {
                    Ok(0) | Err(_) => break,
                    Ok(n) => n,
                };
                if sock.write_all(&buf[..n]).await.is_err() {
                    break;
                }
            }
        });
    }
}

// ── Load generator (runs on a separate plain runtime) ────────────────────────

async fn run_client(
    port: u16,
    running: Arc<AtomicBool>,
    warmup: Arc<AtomicBool>,
) -> Histogram<u64> {
    let mut stream = tokio::net::TcpStream::connect(("127.0.0.1", port))
        .await
        .unwrap();
    let msg = b"hello echo benchmark!";
    let mut buf = [0u8; 256];

    // Warmup phase: send requests but don't record
    while warmup.load(Ordering::Relaxed) {
        stream.write_all(msg).await.unwrap();
        let _ = stream.read(&mut buf).await.unwrap();
    }

    // Measured phase: record into a thread-local histogram
    // Track latencies from 1µs to 60s with 3 significant figures
    let mut hist = Histogram::<u64>::new_with_bounds(1_000, 60_000_000_000, 3).unwrap();
    while running.load(Ordering::Relaxed) {
        let start = Instant::now();
        stream.write_all(msg).await.unwrap();
        let _ = stream.read(&mut buf).await.unwrap();
        let nanos = start.elapsed().as_nanos() as u64;
        // Clamp to histogram bounds
        let nanos = nanos.max(1_000);
        hist.record(nanos).unwrap();
    }

    hist
}

// ── Stats ────────────────────────────────────────────────────────────────────

fn print_stats(hist: &Histogram<u64>, wall: Duration, json: bool) {
    let n = hist.len();
    let rps = n as f64 / wall.as_secs_f64();

    if json {
        println!("{{");
        println!("  \"requests\": {},", n);
        println!("  \"wall_time_secs\": {:.3},", wall.as_secs_f64());
        println!("  \"throughput_rps\": {:.0},", rps);
        println!("  \"mean_lat_ns\": {},", hist.mean() as u64);
        println!("  \"min_lat_ns\": {},", hist.min());
        println!("  \"p50_lat_ns\": {},", hist.value_at_percentile(50.0));
        println!("  \"p90_lat_ns\": {},", hist.value_at_percentile(90.0));
        println!("  \"p99_lat_ns\": {},", hist.value_at_percentile(99.0));
        println!("  \"p99_9_lat_ns\": {},", hist.value_at_percentile(99.9));
        println!("  \"max_lat_ns\": {}", hist.max());
        println!("}}");
    } else {
        println!("  requests : {n}");
        println!("  wall time: {wall:.2?}");
        println!("  throughput: {rps:.0} req/s");
        println!(
            "  mean lat : {:.1?}",
            Duration::from_nanos(hist.mean() as u64)
        );
        println!("  min  lat : {:.1?}", Duration::from_nanos(hist.min()));
        println!(
            "  p50  lat : {:.1?}",
            Duration::from_nanos(hist.value_at_percentile(50.0))
        );
        println!(
            "  p90  lat : {:.1?}",
            Duration::from_nanos(hist.value_at_percentile(90.0))
        );
        println!(
            "  p99  lat : {:.1?}",
            Duration::from_nanos(hist.value_at_percentile(99.0))
        );
        println!(
            "  p99.9 lat: {:.1?}",
            Duration::from_nanos(hist.value_at_percentile(99.9))
        );
        println!("  max  lat : {:.1?}", Duration::from_nanos(hist.max()));
    }
}

// ── Main ─────────────────────────────────────────────────────────────────────

fn main() {
    let args: Vec<String> = std::env::args().collect();
    let json = args.iter().any(|a| a == "--json");

    // Filter out --json to get positional args
    let positional: Vec<&str> = args
        .iter()
        .skip(1)
        .filter(|a| *a != "--json")
        .map(|s| s.as_str())
        .collect();

    let mode = positional.get(0).copied().unwrap_or("baseline");
    let duration_secs: u64 = positional
        .get(1)
        .and_then(|s| s.parse().ok())
        .unwrap_or(DEFAULT_DURATION_SECS);

    if !json {
        println!("mode: {mode}");
        println!(
            "config: {NUM_CLIENTS} clients, {WARMUP_SECS}s warmup, {duration_secs}s measurement"
        );
    }

    let mut builder = tokio::runtime::Builder::new_multi_thread();
    builder.worker_threads(4).enable_all();

    let (server_rt, _guard): (tokio::runtime::Runtime, Option<TelemetryGuard>) = match mode {
        "telemetry" => {
            let writer =
                Box::new(SimpleBinaryWriter::new("/tmp/overhead_bench_trace.bin").unwrap());
            let (rt, g) = TracedRuntime::build_and_start(builder, writer).unwrap();
            (rt, Some(g))
        }
        "noop" => {
            let (rt, g) = TracedRuntime::build_and_start(builder, Box::new(NullWriter)).unwrap();
            (rt, Some(g))
        }
        "baseline" => (builder.build().unwrap(), None),
        other => {
            eprintln!("unknown mode: {other} (expected: baseline, telemetry, noop)");
            std::process::exit(1);
        }
    };

    // Start server and get its port
    let port = server_rt.block_on(async {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let port = listener.local_addr().unwrap().port();
        tokio::spawn(echo_server(listener));
        port
    });

    // ── Build a separate plain runtime for the load generator ──
    let client_rt = tokio::runtime::Builder::new_multi_thread()
        .worker_threads(4)
        .enable_all()
        .build()
        .unwrap();

    // Shared flags to coordinate warmup and measurement phases
    let warmup_flag = Arc::new(AtomicBool::new(true));
    let running_flag = Arc::new(AtomicBool::new(true));

    let wall = Instant::now();

    let (merged_hist, measure_elapsed) = client_rt.block_on(async {
        // Spawn all clients
        let mut handles = Vec::with_capacity(NUM_CLIENTS);
        for _ in 0..NUM_CLIENTS {
            let running = running_flag.clone();
            let warmup = warmup_flag.clone();
            handles.push(tokio::spawn(run_client(port, running, warmup)));
        }

        // Let warmup run
        tokio::time::sleep(Duration::from_secs(WARMUP_SECS)).await;
        if !json {
            println!("warmup complete, starting measurement...");
        }
        warmup_flag.store(false, Ordering::Relaxed);
        let measure_start = Instant::now();

        // Let measurement run for the requested duration
        tokio::time::sleep(Duration::from_secs(duration_secs)).await;
        running_flag.store(false, Ordering::Relaxed);
        let measure_elapsed = measure_start.elapsed();

        // Collect and merge per-client histograms
        let mut merged = Histogram::<u64>::new_with_bounds(1_000, 60_000_000_000, 3).unwrap();
        for h in handles {
            let client_hist = h.await.unwrap();
            merged.add(&client_hist).unwrap();
        }
        (merged, measure_elapsed)
    });

    drop(client_rt);
    drop(server_rt);

    if !json {
        println!("\n── results ({mode}) ──");
    }
    print_stats(&merged_hist, measure_elapsed, json);
    if !json {
        println!("  total wall (incl. warmup): {:.2?}", wall.elapsed());
    }
}
