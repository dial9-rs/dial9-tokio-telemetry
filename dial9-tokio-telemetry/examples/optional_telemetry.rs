//! Optional / best-effort telemetry via `build_or_disabled`.
//!
//! `Dial9Config::builder().build_or_disabled()` returns a
//! `Dial9ConfigFallback` that never reports a build error: missing
//! required writer fields, an unwritable `base_path`, or telemetry-core
//! I/O failures all cascade silently into a plain tokio runtime
//! carrying the user's `with_tokio` configurators. Use this when
//! telemetry is best-effort and you'd rather keep your service
//! starting than crash on a writer setup problem.
//!
//! Usage:
//!   cargo run --example optional_telemetry
//!
//! Force the lenient cascade by pointing at an unwritable path:
//!   DIAL9_TRACE_PATH=/this/dir/does/not/exist/trace.bin \
//!     cargo run --example optional_telemetry
//!
//! Disable telemetry up front by leaving the path unset and clearing
//! the writer fields the builder normally requires (the
//! `build_or_disabled` path also tolerates that — it just yields a
//! disabled runtime).

use std::time::Duration;

use dial9_tokio_telemetry::Dial9Config;
use dial9_tokio_telemetry::telemetry::TelemetryHandle;

fn config() -> dial9_tokio_telemetry::Dial9ConfigFallback {
    let trace_path = std::env::var("DIAL9_TRACE_PATH")
        .unwrap_or_else(|_| "optional_telemetry_trace.bin".to_string());

    Dial9Config::builder()
        .base_path(trace_path)
        .max_file_size(64 * 1024 * 1024)
        .max_total_size(256 * 1024 * 1024)
        .with_tokio(|t| {
            t.worker_threads(2);
        })
        .build_or_disabled()
}

#[dial9_tokio_telemetry::main(config = config)]
async fn main() {
    match TelemetryHandle::try_current() {
        Some(_) => println!("telemetry: ENABLED (trace will be written)"),
        None => println!("telemetry: DISABLED (cascade fell back to plain tokio runtime)"),
    }

    let tasks: Vec<_> = (0..8)
        .map(|i| {
            tokio::spawn(async move {
                tokio::time::sleep(Duration::from_millis(5)).await;
                println!("task {i} done");
            })
        })
        .collect();

    for t in tasks {
        let _ = t.await;
    }
}
