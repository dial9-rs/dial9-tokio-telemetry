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
    // `handle` is injected by the macro — use it to spawn a sub-task
    // with wake-event tracking.
    let sub = handle.spawn(async { 7 + 3 });
    sub.await.unwrap()
}

#[test]
fn macro_exposes_handle_for_nested_spawn() {
    let result = with_nested_spawn();
    assert_eq!(result, 10);
}
