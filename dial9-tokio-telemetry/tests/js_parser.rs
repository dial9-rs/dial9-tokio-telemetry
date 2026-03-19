//! Integration test: verify JS trace parser matches Rust parser

use dial9_tokio_telemetry::telemetry::{RotatingWriter, TraceReader, TracedRuntime};
use std::io::{BufWriter, Write};
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

    // Generate a trace — enable CPU profiling on Linux where it's available
    {
        let mut builder = tokio::runtime::Builder::new_multi_thread();
        builder.worker_threads(2).enable_all();

        let writer = RotatingWriter::single_file(&trace_path).unwrap();
        #[allow(unused_mut)]
        let mut tb = TracedRuntime::builder().with_task_tracking(true);
        #[cfg(feature = "cpu-profiling")]
        {
            tb = tb.with_cpu_profiling(
                dial9_tokio_telemetry::telemetry::CpuProfilingConfig::default(),
            );
        }
        let (runtime, _guard) = tb.build_and_start(builder, writer).unwrap();

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

    // Export to JSONL using Rust parser (in-process to avoid cargo subprocess overhead)
    {
        let mut reader = TraceReader::new(trace_path.to_str().unwrap()).unwrap();
        reader.read_header().unwrap();
        let file = std::fs::File::create(&jsonl_path).unwrap();
        let mut w = BufWriter::new(file);
        while let Some(e) = reader.read_raw_event().unwrap() {
            serde_json::to_writer(&mut w, &e).unwrap();
            w.write_all(b"\n").unwrap();
        }
        w.flush().unwrap();
    }

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
