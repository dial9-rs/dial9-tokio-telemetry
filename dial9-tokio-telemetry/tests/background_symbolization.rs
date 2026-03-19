//! Integration tests for background symbolization.
//!
//! Verifies that when cpu-profiling is configured with a trace path,
//! the background worker symbolizes sealed segments and writes them
//! back to disk with gzip compression.
#![cfg(feature = "cpu-profiling")]

use dial9_tokio_telemetry::telemetry::{CpuProfilingConfig, RotatingWriter, TracedRuntime};
use dial9_trace_format::decoder::Decoder;
use flate2::read::GzDecoder;
use std::io::Read;

/// Burn CPU in a tight loop to generate stack samples.
///
/// Marked `#[inline(never)]` so the function appears in stack traces
/// with a stable symbol name.
#[inline(never)]
fn burn_cpu_work() {
    let mut x: u64 = 1;
    for i in 0..5_000_000u64 {
        x = x.wrapping_mul(i | 1).wrapping_add(7);
    }
    // Prevent the loop from being optimized away.
    std::hint::black_box(x);
}

/// Build a TracedRuntime with cpu-profiling, run a CPU-burning workload,
/// shut down gracefully, then read the symbolized segments and verify
/// that SymbolTableEntry events contain real resolved symbol names.
#[test]
fn background_symbolization_produces_symbol_table_entries() {
    let trace_dir = tempfile::tempdir().unwrap();
    let trace_path = trace_dir.path().join("trace.bin");

    // Small segments to force rotation so the worker has segments to process.
    // Large total size so segments aren't evicted before the worker processes them.
    let writer = RotatingWriter::new(&trace_path, 4 * 1024, 10 * 1024 * 1024).unwrap();

    let mut builder = tokio::runtime::Builder::new_multi_thread();
    builder.worker_threads(2).enable_all();

    let (runtime, guard) = TracedRuntime::builder()
        .with_cpu_profiling(CpuProfilingConfig {
            frequency_hz: 999,
            ..Default::default()
        })
        .with_trace_path(&trace_path)
        .with_worker_poll_interval(std::time::Duration::from_millis(50))
        .build_and_start(builder, writer)
        .unwrap();

    // Burn CPU across multiple threads to generate CpuSample events.
    runtime.block_on(async {
        let mut handles = Vec::new();
        for _ in 0..4 {
            handles.push(tokio::spawn(tokio::task::spawn_blocking(burn_cpu_work)));
        }
        for h in handles {
            let _ = h.await;
        }
        // Give the flush thread time to write and seal segments,
        // and the worker time to symbolize them.
        tokio::time::sleep(std::time::Duration::from_secs(3)).await;

        // Graceful shutdown: seals final segment, worker drains all remaining.
        guard
            .graceful_shutdown(std::time::Duration::from_secs(10))
            .await
            .expect("graceful shutdown");
    });

    drop(runtime);

    // Read all .bin files in the trace directory. After the worker runs,
    // processed segments are gzip-compressed (GzipWriteBackProcessor).
    let mut all_symbol_names: Vec<String> = Vec::new();
    let mut all_source_files: Vec<String> = Vec::new();

    for entry in std::fs::read_dir(trace_dir.path()).unwrap() {
        let entry = entry.unwrap();
        let path = entry.path();
        if path.extension().is_none_or(|ext| ext != "bin") {
            continue;
        }

        let raw = std::fs::read(&path).unwrap();
        if raw.is_empty() {
            continue;
        }

        // Try gzip decompression first (worker-processed segments).
        // Fall back to raw bytes (unprocessed segments).
        let bytes = match decompress_gzip(&raw) {
            Some(decompressed) => decompressed,
            None => raw,
        };

        let Some(mut dec) = Decoder::new(&bytes) else {
            continue;
        };
        dec.for_each_event(|ev| {
            if ev.name == "SymbolTableEntry"
                && let Some(dial9_trace_format::types::FieldValueRef::PooledString(id)) =
                    ev.fields.get(2)
                && let Some(name) = ev.string_pool.get(*id)
            {
                let source_file =
                    if let Some(dial9_trace_format::types::FieldValueRef::PooledString(fid)) =
                        ev.fields.get(4)
                        && let Some(f) = ev.string_pool.get(*fid)
                        && !f.is_empty()
                    {
                        f.to_string()
                    } else {
                        String::new()
                    };
                if name.contains("burn_cpu_work") && !source_file.is_empty() {
                    all_source_files.push(source_file);
                }
                all_symbol_names.push(name.to_string());
            }
        })
        .ok();
    }

    assert!(
        !all_symbol_names.is_empty(),
        "expected SymbolTableEntry events with resolved symbol names, found none"
    );

    // Verify at least one symbol name contains our burn function.
    let has_burn = all_symbol_names
        .iter()
        .any(|name| name.contains("burn_cpu_work"));
    assert!(
        has_burn,
        "expected to find 'burn_cpu_work' in symbol names, got: {:?}",
        &all_symbol_names[..all_symbol_names.len().min(20)]
    );

    // Verify that burn_cpu_work entries have source file locations (code_info).
    assert!(
        !all_source_files.is_empty(),
        "expected burn_cpu_work SymbolTableEntry to have source_file, but none did"
    );
    assert!(
        all_source_files
            .iter()
            .any(|f| f.ends_with("background_symbolization.rs")),
        "expected source_file to reference this test file, got: {:?}",
        &all_source_files
    );
}

fn decompress_gzip(data: &[u8]) -> Option<Vec<u8>> {
    let mut decoder = GzDecoder::new(data);
    let mut buf = Vec::new();
    decoder.read_to_end(&mut buf).ok()?;
    Some(buf)
}
