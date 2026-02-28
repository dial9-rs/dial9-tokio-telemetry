//! Integration test: verify JS trace parser matches Rust parser

use dial9_tokio_telemetry::telemetry::{CpuProfilingConfig, SimpleBinaryWriter, TracedRuntime};
use std::process::Command;
use tempfile::TempDir;

#[inline(never)]
fn burn_cpu(iterations: u64) -> u64 {
    let mut result = 0u64;
    for i in 0..iterations {
        result = result.wrapping_add(i.wrapping_mul(i));
    }
    result
}

async fn cpu_task(id: usize) {
    for _ in 0..3 {
        let _ = burn_cpu(1_000_000);
        tokio::task::yield_now().await;
    }
    eprintln!("Task {id} done");
}

#[test]
fn test_js_parser_matches_rust() {
    let temp_dir = TempDir::new().unwrap();
    let trace_path = temp_dir.path().join("test_trace.bin");
    let jsonl_path = temp_dir.path().join("expected.jsonl");

    // Generate a trace with CPU profiling
    {
        let mut builder = tokio::runtime::Builder::new_multi_thread();
        builder.worker_threads(2).enable_all();

        let writer = Box::new(SimpleBinaryWriter::new(&trace_path).unwrap());
        let (runtime, _guard) = TracedRuntime::builder()
            .with_task_tracking(true)
            .with_cpu_profiling(CpuProfilingConfig::default())
            .with_inline_callframe_symbols(true)
            .build_and_start(builder, writer)
            .unwrap();

        runtime.block_on(async {
            let mut tasks = vec![];
            for i in 0..10 {
                tasks.push(tokio::spawn(cpu_task(i)));
            }
            for task in tasks {
                let _ = task.await;
            }
        });
    }

    eprintln!("Generated trace at {}", trace_path.display());

    // Export to JSONL using Rust parser
    let output = Command::new("cargo")
        .args([
            "run",
            "--example",
            "trace_to_jsonl",
            trace_path.to_str().unwrap(),
            jsonl_path.to_str().unwrap(),
        ])
        .output()
        .expect("Failed to run trace_to_jsonl");

    assert!(
        output.status.success(),
        "trace_to_jsonl failed: {}",
        String::from_utf8_lossy(&output.stderr)
    );

    eprintln!("Exported JSONL to {}", jsonl_path.display());

    // Run JS parser test (use CARGO_MANIFEST_DIR to find trace_viewer)
    let manifest_dir = std::env::var("CARGO_MANIFEST_DIR").unwrap();
    let test_script = std::path::Path::new(&manifest_dir)
        .join("trace_viewer")
        .join("test_parser.js");

    let test_output = Command::new("node")
        .args([
            test_script.to_str().unwrap(),
            trace_path.to_str().unwrap(),
            jsonl_path.to_str().unwrap(),
        ])
        .output()
        .expect("Failed to run node test_parser.js");

    eprintln!("{}", String::from_utf8_lossy(&test_output.stdout));

    assert!(
        test_output.status.success(),
        "JS parser test failed:\n{}",
        String::from_utf8_lossy(&test_output.stderr)
    );
}
