//! Integration test: sched event capture via per-thread perf profiling.

#[cfg(feature = "cpu-profiling")]
#[test]
fn sched_events_capture_context_switches() {
    use dial9_tokio_telemetry::telemetry::events::TelemetryEvent;
    use dial9_tokio_telemetry::telemetry::events::CpuSampleSource;
    use dial9_tokio_telemetry::telemetry::writer::TraceWriter;
    use dial9_tokio_telemetry::telemetry::{SchedEventConfig, TracedRuntime};
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

    let num_workers = 2;
    let mut builder = tokio::runtime::Builder::new_multi_thread();
    builder.worker_threads(num_workers).enable_all();

    let (runtime, guard) = TracedRuntime::builder()
        .with_sched_events(SchedEventConfig::default())
        .with_inline_callframe_symbols(true)
        .build_and_start(builder, Box::new(writer))
        .unwrap();

    runtime.block_on(async {
        let mut handles = Vec::new();
        for _ in 0..num_workers * 2 {
            handles.push(tokio::spawn(async {
                std::thread::sleep(Duration::from_millis(10));
            }));
        }
        for h in handles {
            h.await.unwrap();
        }
        tokio::time::sleep(Duration::from_millis(500)).await;
    });

    drop(runtime);
    drop(guard);

    let events = events.lock().unwrap();

    // 1. CpuSample events exist with SchedEvent source and some are attributed to workers
    let worker_samples: Vec<_> = events
        .iter()
        .filter(|e| matches!(e, TelemetryEvent::CpuSample { worker_id, source, .. }
            if *worker_id < num_workers && *source == CpuSampleSource::SchedEvent))
        .collect();
    assert!(
        !worker_samples.is_empty(),
        "expected CpuSample events with source=SchedEvent attributed to workers"
    );

    // No samples should have CpuProfile source (we didn't enable cpu profiling)
    let cpu_profile_samples = events
        .iter()
        .filter(|e| matches!(e, TelemetryEvent::CpuSample { source, .. }
            if *source == CpuSampleSource::CpuProfile))
        .count();
    assert_eq!(cpu_profile_samples, 0, "should have no CpuProfile samples");

    // 2. CallframeDef symbols exist, contain "sleep", and include line numbers
    let symbols: Vec<&str> = events
        .iter()
        .filter_map(|e| match e {
            TelemetryEvent::CallframeDef { symbol, .. } => Some(symbol.as_str()),
            _ => None,
        })
        .collect();

    assert!(!symbols.is_empty(), "expected CallframeDef events");

    assert!(
        symbols.iter().any(|s| s.contains("sleep") || s.contains("nanosleep")),
        "expected a symbol containing 'sleep', got:\n{}",
        symbols.join("\n")
    );

    // Line numbers show up as "(path:NNN)" in the symbol string
    assert!(
        symbols.iter().any(|s| {
            s.contains('(')
                && s.split('(').last().and_then(|p| p.strip_suffix(')')).is_some_and(|loc| {
                    loc.rsplit(':').next().is_some_and(|n| n.parse::<u32>().is_ok())
                })
        }),
        "expected at least one symbol with a line number like 'name (file:123)', got:\n{}",
        symbols.join("\n")
    );
}
