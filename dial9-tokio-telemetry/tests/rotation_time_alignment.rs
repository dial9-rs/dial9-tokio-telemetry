//! Verify that rotated trace segments contain events from non-overlapping
//! (or minimally overlapping) time ranges.
//!
//! When rotation and flushing are properly coordinated, each segment should
//! contain events from a contiguous time window. Adjacent segments may overlap
//! by at most a small tolerance (e.g. 2 seconds) due to in-flight batches.
//!
//! This test uses a short rotation period (3s) and generates continuous events
//! across multiple workers to exercise the rotation/flush coordination path.

use dial9_tokio_telemetry::telemetry::{RotatingWriter, TelemetryEvent, TracedRuntime};
use std::time::Duration;

/// Maximum allowed overlap between adjacent segments in seconds.
const MAX_OVERLAP_SECS: f64 = 2.0;

#[test]
fn rotated_segments_have_bounded_time_overlap() {
    let dir = tempfile::tempdir().unwrap();
    let trace_path = dir.path().join("trace.bin");

    let rotation_period = Duration::from_secs(3);
    let num_workers = 4;

    let writer = RotatingWriter::builder()
        .base_path(&trace_path)
        .max_file_size(u64::MAX) // only time-based rotation
        .max_total_size(u64::MAX)
        .rotation_period(rotation_period)
        .build()
        .unwrap();

    let mut builder = tokio::runtime::Builder::new_multi_thread();
    builder.worker_threads(num_workers).enable_all();

    let (runtime, guard) = TracedRuntime::build_and_start(builder, writer).unwrap();

    // Generate continuous events across multiple rotation boundaries.
    // With a 3s rotation period, running for ~10s should produce 3+ segments.
    runtime.block_on(async {
        let mut handles = Vec::new();
        for _ in 0..num_workers {
            handles.push(tokio::spawn(async {
                for _ in 0..500 {
                    tokio::task::yield_now().await;
                    tokio::time::sleep(Duration::from_millis(20)).await;
                }
            }));
        }
        for h in handles {
            h.await.unwrap();
        }
    });

    drop(runtime);
    drop(guard);

    // Collect all sealed segment files, sorted by index.
    let mut segment_files: Vec<_> = std::fs::read_dir(dir.path())
        .unwrap()
        .filter_map(|e| e.ok())
        .map(|e| e.path())
        .filter(|p| {
            p.extension().is_some_and(|ext| ext == "bin")
                && !p
                    .file_name()
                    .unwrap()
                    .to_str()
                    .unwrap()
                    .ends_with(".active")
        })
        .collect();
    segment_files.sort();

    assert!(
        segment_files.len() >= 3,
        "expected at least 3 rotated segments, got {}. Files: {:?}",
        segment_files.len(),
        segment_files
    );

    // For each segment, compute (min_timestamp, max_timestamp) from non-metadata events.
    let segment_ranges: Vec<(u64, u64)> = segment_files
        .iter()
        .map(|path| {
            let data = std::fs::read(path).unwrap();
            let events = dial9_tokio_telemetry::analysis_unstable::decode_events(&data).unwrap();
            let timestamps: Vec<u64> = events
                .iter()
                .filter(|e| !matches!(e, TelemetryEvent::SegmentMetadata { .. }))
                .filter_map(|e| e.timestamp_nanos())
                .collect();
            assert!(
                !timestamps.is_empty(),
                "segment {} has no timestamped events",
                path.display()
            );
            let min = *timestamps.iter().min().unwrap();
            let max = *timestamps.iter().max().unwrap();
            (min, max)
        })
        .collect();

    // Validate: adjacent segments should have bounded overlap.
    // Skip the last boundary — the final segment is the shutdown dump where
    // all TL buffers are drained at once, so it inherently contains events
    // spanning the entire last drain interval.
    let mut max_observed_overlap = Duration::ZERO;
    assert!(
        segment_ranges.len() >= 4,
        "need at least 4 segments to validate non-final boundaries, got {}",
        segment_ranges.len()
    );
    for i in 0..segment_ranges.len() - 2 {
        let (_min_a, max_a) = segment_ranges[i];
        let (min_b, _max_b) = segment_ranges[i + 1];

        // Overlap = how much of segment A's tail extends past segment B's start.
        // If max_a > min_b, there's overlap.
        let overlap = if max_a > min_b {
            Duration::from_nanos(max_a - min_b)
        } else {
            Duration::ZERO
        };

        if overlap > max_observed_overlap {
            max_observed_overlap = overlap;
        }

        let overlap_secs = overlap.as_secs_f64();
        eprintln!(
            "segments {i} → {}: overlap = {:.3}s (segment {i}: [{:.3}s, {:.3}s], segment {}: [{:.3}s, {:.3}s])",
            i + 1,
            overlap_secs,
            segment_ranges[i].0 as f64 / 1e9,
            segment_ranges[i].1 as f64 / 1e9,
            i + 1,
            segment_ranges[i + 1].0 as f64 / 1e9,
            segment_ranges[i + 1].1 as f64 / 1e9,
        );

        assert!(
            overlap_secs <= MAX_OVERLAP_SECS,
            "segment {i} → {} overlap is {:.3}s, exceeds {MAX_OVERLAP_SECS}s tolerance. \
             Segment {i} max={}, segment {} min={}",
            i + 1,
            overlap_secs,
            max_a,
            i + 1,
            min_b,
        );
    }

    eprintln!(
        "max observed overlap: {:.3}s across {} non-final segment boundaries",
        max_observed_overlap.as_secs_f64(),
        segment_ranges.len() - 2
    );
}
