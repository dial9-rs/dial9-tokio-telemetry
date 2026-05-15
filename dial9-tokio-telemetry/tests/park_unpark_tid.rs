//! Test that WorkerParkEvent and WorkerUnparkEvent include a non-zero tid field.

mod common;

use dial9_tokio_telemetry::telemetry::{TelemetryEvent, TracedRuntime};

#[test]
#[cfg(feature = "analysis")]
fn worker_park_unpark_events_carry_nonzero_tid() {
    let (writer, events) = common::CapturingWriter::new();

    let mut builder = tokio::runtime::Builder::new_multi_thread();
    builder.worker_threads(2).enable_all();
    let (runtime, guard) = TracedRuntime::builder()
        .build_and_start_with_writer(builder, writer)
        .unwrap();

    // Generate park/unpark cycles by spawning work that yields
    runtime.block_on(async {
        let mut handles = Vec::new();
        for _ in 0..20 {
            handles.push(tokio::spawn(async {
                tokio::task::yield_now().await;
            }));
        }
        for h in handles {
            h.await.unwrap();
        }
        // Sleep briefly to ensure workers park
        tokio::time::sleep(std::time::Duration::from_millis(50)).await;
    });

    drop(runtime);
    drop(guard);

    let events = events.lock().unwrap();

    let park_tids: Vec<u32> = events
        .iter()
        .filter_map(|e| match e {
            TelemetryEvent::WorkerPark { tid, .. } => Some(*tid),
            _ => None,
        })
        .collect();

    let unpark_tids: Vec<u32> = events
        .iter()
        .filter_map(|e| match e {
            TelemetryEvent::WorkerUnpark { tid, .. } => Some(*tid),
            _ => None,
        })
        .collect();

    assert!(
        !park_tids.is_empty(),
        "expected at least one WorkerPark event"
    );
    assert!(
        !unpark_tids.is_empty(),
        "expected at least one WorkerUnpark event"
    );

    for tid in &park_tids {
        assert_ne!(*tid, 0, "WorkerPark tid must be non-zero");
    }
    for tid in &unpark_tids {
        assert_ne!(*tid, 0, "WorkerUnpark tid must be non-zero");
    }
}
