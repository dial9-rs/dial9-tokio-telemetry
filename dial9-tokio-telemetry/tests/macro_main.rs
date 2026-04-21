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
    Dial9Config::builder()
        .base_path(tmp_base_path())
        .max_file_size(1024 * 1024)
        .max_total_size(4 * 1024 * 1024)
        .build()
        .expect("config build failed")
}

fn disabled_config() -> Dial9Config {
    Dial9Config::builder()
        .enabled(false)
        .with_tokio(|t| {
            t.worker_threads(2);
        })
        .build()
        .expect("config build failed")
}

#[dial9_tokio_telemetry::main(config = test_config)]
async fn runs_async_body() {
    tokio::time::sleep(std::time::Duration::from_millis(1)).await;
}

#[test]
fn macro_runs_async_body() {
    runs_async_body();
}

#[dial9_tokio_telemetry::main(config = || {
    Dial9Config::builder()
        .base_path(tmp_base_path())
        .max_file_size(1024 * 1024)
        .max_total_size(4 * 1024 * 1024)
        .build()
        .expect("config build failed")
})]
async fn runs_with_inline_closure() {
    tokio::time::sleep(std::time::Duration::from_millis(1)).await;
}

#[test]
fn macro_runs_with_inline_closure() {
    runs_with_inline_closure();
}

#[dial9_tokio_telemetry::main(config = move || {
    let path = tmp_base_path();
    Dial9Config::builder()
        .base_path(path)
        .max_file_size(1024 * 1024)
        .max_total_size(4 * 1024 * 1024)
        .build()
        .expect("config build failed")
})]
async fn runs_with_move_closure() {
    tokio::time::sleep(std::time::Duration::from_millis(1)).await;
}

#[test]
fn macro_runs_with_move_closure() {
    runs_with_move_closure();
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

// --- Disabled telemetry ---

fn disabled_config_default() -> Dial9Config {
    Dial9Config::builder()
        .enabled(false)
        .build()
        .expect("config build failed")
}

#[dial9_tokio_telemetry::main(config = disabled_config)]
async fn runs_without_telemetry() -> i32 {
    tokio::spawn(async { 123 }).await.unwrap()
}

#[test]
fn macro_runs_with_disabled_config() {
    let result = runs_without_telemetry();
    assert_eq!(result, 123);
}

#[dial9_tokio_telemetry::main(config = disabled_config_default)]
async fn disabled_default_runs() -> i32 {
    tokio::spawn(async { 99 }).await.unwrap()
}

#[test]
fn macro_runs_with_disabled_default() {
    assert_eq!(disabled_default_runs(), 99);
}

#[dial9_tokio_telemetry::main(config = disabled_config)]
async fn disabled_with_return_type() -> Result<i32, Box<dyn std::error::Error + Send + Sync>> {
    let val = tokio::spawn(async { 42 }).await?;
    Ok(val)
}

#[test]
fn macro_disabled_preserves_return_type() {
    assert_eq!(disabled_with_return_type().unwrap(), 42);
}

#[dial9_tokio_telemetry::main(config = disabled_config)]
async fn disabled_no_telemetry_handle() -> bool {
    // TelemetryHandle should not be available when disabled.
    dial9_tokio_telemetry::telemetry::TelemetryHandle::try_current().is_none()
}

#[test]
fn macro_disabled_has_no_telemetry_handle() {
    assert!(disabled_no_telemetry_handle());
}

#[dial9_tokio_telemetry::main(config = disabled_config)]
async fn disabled_timers_work() {
    tokio::time::sleep(std::time::Duration::from_millis(1)).await;
}

#[test]
fn macro_disabled_timers_work() {
    disabled_timers_work();
}

#[dial9_tokio_telemetry::main(config = disabled_config)]
async fn disabled_nested_spawn() -> i32 {
    let inner = tokio::spawn(async { tokio::spawn(async { 7 + 3 }).await.unwrap() });
    inner.await.unwrap()
}

#[test]
fn macro_disabled_nested_spawn() {
    assert_eq!(disabled_nested_spawn(), 10);
}
