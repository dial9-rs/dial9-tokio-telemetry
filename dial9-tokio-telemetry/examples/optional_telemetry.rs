//! Optional / best-effort telemetry via `build_or_disabled`.
//!
//! `Dial9Config::builder().build_or_disabled()` returns a
//! [`Dial9Config`] that never reports a build error: missing required
//! writer fields and writer-I/O probe failures (e.g. an unwritable
//! `base_path`) are logged at `error!` level and downgraded silently
//! into a plain tokio runtime carrying the user's `with_tokio`
//! configurators. Use this when telemetry is best-effort and you'd
//! rather keep your service starting than crash on a writer setup
//! problem.
//!
//! Usage:
//!   cargo run --example optional_telemetry
//!
//! Force the lenient downgrade by pointing at an unwritable path
//! (you'll see an `error!` log and the runtime will start without
//! telemetry):
//!   DIAL9_TRACE_PATH=/this/dir/does/not/exist/trace.bin \
//!     cargo run --example optional_telemetry

use std::time::Duration;

use dial9_tokio_telemetry::Dial9Config;
use dial9_tokio_telemetry::telemetry::TelemetryHandle;

fn config() -> Dial9Config {
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
    if TelemetryHandle::current().is_enabled() {
        println!("telemetry: ENABLED (trace will be written)");
    } else {
        println!("telemetry: DISABLED (downgraded to a plain tokio runtime)");
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
