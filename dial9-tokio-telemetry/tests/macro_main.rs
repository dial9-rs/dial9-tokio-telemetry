use std::panic::{AssertUnwindSafe, catch_unwind};
use std::path::PathBuf;

use dial9_tokio_telemetry::config::Dial9Config;

fn tmp_base_path() -> PathBuf {
    let dir = tempfile::tempdir().expect("tempdir");
    let path = dir.path().join("trace.bin");
    std::mem::forget(dir);
    path
}

fn test_config() -> Dial9Config {
    Dial9Config::new(tmp_base_path(), 1024 * 1024, 4 * 1024 * 1024)
}

#[dial9_tokio_telemetry::main(config = test_config)]
async fn runs_async_body() {
    tokio::time::sleep(std::time::Duration::from_millis(1)).await;
}

#[test]
fn macro_runs_async_body() {
    runs_async_body();
}

#[dial9_tokio_telemetry::main(config = test_config)]
async fn with_return_type() -> Result<i32, Box<dyn std::error::Error + Send + Sync>> {
    let val = tokio::spawn(async { 42 }).await?;
    Ok(val)
}

#[test]
fn macro_preserves_return_type() {
    let result = with_return_type();
    assert_eq!(result.unwrap(), 42);
}

#[dial9_tokio_telemetry::main(config = test_config)]
async fn with_nested_spawn() -> i32 {
    // `TelemetryHandle::current()` is populated by `on_thread_start` on
    // every runtime-owned thread — use it to spawn instrumented sub-tasks.
    let handle = dial9_tokio_telemetry::telemetry::TelemetryHandle::current();
    let sub = handle.spawn(async { 7 + 3 });
    sub.await.unwrap()
}

#[test]
fn macro_exposes_handle_for_nested_spawn() {
    let result = with_nested_spawn();
    assert_eq!(result, 10);
}

// --- Error propagation ---

#[dial9_tokio_telemetry::main(config = test_config)]
async fn body_returns_err() -> Result<(), String> {
    Err("something went wrong".into())
}

#[test]
fn macro_propagates_err_variant() {
    let result = body_returns_err();
    assert_eq!(result.unwrap_err(), "something went wrong");
}

#[dial9_tokio_telemetry::main(config = test_config)]
async fn body_returns_custom_err() -> Result<i32, std::io::Error> {
    Err(std::io::Error::new(std::io::ErrorKind::NotFound, "missing"))
}

#[test]
fn macro_propagates_io_error() {
    let result = body_returns_custom_err();
    let err = result.unwrap_err();
    assert_eq!(err.kind(), std::io::ErrorKind::NotFound);
    assert_eq!(err.to_string(), "missing");
}

// --- Panic propagation ---

#[dial9_tokio_telemetry::main(config = test_config)]
async fn body_panics_with_str() {
    panic!("boom");
}

#[test]
fn macro_propagates_panic_payload() {
    let result = catch_unwind(AssertUnwindSafe(body_panics_with_str));
    let payload = result.expect_err("should have panicked");
    let msg = payload
        .downcast_ref::<&str>()
        .expect("payload should be &str");
    assert_eq!(*msg, "boom");
}

#[dial9_tokio_telemetry::main(config = test_config)]
#[allow(clippy::unnecessary_literal_unwrap)]
async fn body_panics_with_format() {
    let x: Option<i32> = None;
    x.unwrap();
}

#[test]
fn macro_propagates_unwrap_panic() {
    let result = catch_unwind(AssertUnwindSafe(body_panics_with_format));
    let payload = result.expect_err("should have panicked");
    let msg = payload
        .downcast_ref::<&str>()
        .map(|s| s.to_string())
        .or_else(|| payload.downcast_ref::<String>().cloned())
        .expect("payload should be &str or String");
    assert!(msg.contains("None"), "expected unwrap message, got: {msg}");
}
