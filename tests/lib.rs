// Integration tests test your crate's public API. They only have access to items
// in your crate that are marked pub. See the Cargo Targets page of the Cargo Book
// for more information.
//
//   https://doc.rust-lang.org/cargo/reference/cargo-targets.html#integration-tests
//
#[test]
fn integration_test() {
    // Basic integration test
    assert!(true);
}

#[test]
fn test_worker_ids_match_tokio() {
    use dial9_tokio_telemetry::telemetry::TracedRuntime;
    use dial9_tokio_telemetry::telemetry::events::TelemetryEvent;
    use dial9_tokio_telemetry::telemetry::writer::TraceWriter;
    use std::sync::{Arc, Mutex};
    use std::time::Duration;

    struct CapturingWriter(Arc<Mutex<Vec<TelemetryEvent>>>);
    impl TraceWriter for CapturingWriter {
        fn write_event(&mut self, event: &TelemetryEvent) -> std::io::Result<()> {
            self.0.lock().unwrap().push(event.clone());
            Ok(())
        }
        fn write_batch(&mut self, events: &[TelemetryEvent]) -> std::io::Result<()> {
            self.0.lock().unwrap().extend_from_slice(events);
            Ok(())
        }
        fn flush(&mut self) -> std::io::Result<()> {
            Ok(())
        }
    }

    let events = Arc::new(Mutex::new(Vec::new()));
    let writer = CapturingWriter(events.clone());

    let num_workers = 4;
    let mut builder = tokio::runtime::Builder::new_multi_thread();
    builder.worker_threads(num_workers).enable_all();

    let (runtime, guard) = TracedRuntime::builder()
        .build_and_start(builder, Box::new(writer))
        .unwrap();

    runtime.block_on(async {
        // Spawn work on each worker to generate events
        let mut handles = Vec::new();
        for _ in 0..num_workers * 4 {
            handles.push(tokio::spawn(async {
                tokio::time::sleep(Duration::from_millis(10)).await;
            }));
        }
        for h in handles {
            h.await.unwrap();
        }
        // Let flush cycle run
        tokio::time::sleep(Duration::from_millis(500)).await;
    });

    drop(guard);

    let events = events.lock().unwrap();
    assert!(!events.is_empty(), "should have captured events");

    for event in events.iter() {
        // Only check worker_id on events that have one;
        // QueueSample, SpawnLocationDef, and TaskSpawn don't carry a worker_id.
        if let Some(worker_id) = event.worker_id() {
            assert!(
                worker_id < num_workers,
                "worker_id {} should be < num_workers {} (event: {:?})",
                worker_id,
                num_workers,
                event,
            );
        }
    }
}
