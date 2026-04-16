//! Example: emitting custom events into a dial9 trace.
//!
//! Shows two patterns:
//! 1. **Simple** — `#[derive(TraceEvent)]` struct passed directly to `record_event`
//! 2. **Advanced** — manual `Encodable` impl with string interning for repeated values
//!
//! With the `analysis` feature, also demonstrates reading custom events back
//! through `TraceReader` and resolving interned strings.
//!
//! Run with:
//! ```sh
//! cargo run --example custom_events --features analysis
//! ```

use dial9_tokio_telemetry::telemetry::{
    Encodable, RotatingWriter, ThreadLocalEncoder, TracedRuntime, clock_monotonic_ns, record_event,
};
use dial9_trace_format::{InternedString, TraceEvent};
use std::time::Duration;

// ── Simple: derive-only, no interning ───────────────────────────────────────

/// A custom event with only primitive fields. The blanket `Encodable` impl
/// handles encoding automatically — just pass it to `record_event`.
#[derive(TraceEvent)]
struct RequestCompleted {
    #[traceevent(timestamp)]
    timestamp_ns: u64,
    status_code: u32,
    latency_us: u64,
}

// ── Advanced: manual Encodable with string interning ────────────────────────

/// Application-level event with a string field we want to intern.
struct HttpRequest {
    timestamp_ns: u64,
    method: String,
    status: u32,
}

/// Wire-format struct with the interned string handle.
#[derive(TraceEvent)]
struct HttpRequestWire {
    #[traceevent(timestamp)]
    timestamp_ns: u64,
    method: InternedString,
    status: u32,
}

impl Encodable for HttpRequest {
    fn encode(&self, enc: &mut ThreadLocalEncoder<'_>) {
        let method = enc.intern_string(&self.method);
        enc.encode(&HttpRequestWire {
            timestamp_ns: self.timestamp_ns,
            method,
            status: self.status,
        });
    }
}

fn main() -> std::io::Result<()> {
    let dir = tempfile::tempdir()?;
    let trace_path = dir.path().join("trace.bin");

    let writer = RotatingWriter::single_file(&trace_path)?;
    let mut builder = tokio::runtime::Builder::new_multi_thread();
    builder.worker_threads(2).enable_all();

    let (runtime, guard) = TracedRuntime::build_and_start(builder, writer)?;
    let handle = guard.handle();

    runtime.block_on(async {
        // Simple: derive-only events
        for i in 0..10 {
            record_event(
                RequestCompleted {
                    timestamp_ns: clock_monotonic_ns(),
                    status_code: 200,
                    latency_us: 100 + i,
                },
                &handle,
            );
        }

        // Advanced: manual Encodable with interning
        // "GET" is interned once and reused across all events in the batch.
        for _ in 0..10 {
            record_event(
                HttpRequest {
                    timestamp_ns: clock_monotonic_ns(),
                    method: "GET".into(),
                    status: 200,
                },
                &handle,
            );
        }

        tokio::time::sleep(Duration::from_millis(100)).await;
    });

    drop(runtime);
    drop(guard);

    // Verify: decode the trace and count our custom events
    let sealed = dir.path().join("trace.0.bin");
    let data = std::fs::read(&sealed)?;
    let mut decoder = dial9_trace_format::decoder::Decoder::new(&data)
        .ok_or_else(|| std::io::Error::other("invalid trace"))?;

    let mut request_completed = 0u32;
    let mut http_request = 0u32;
    decoder
        .for_each_event(|ev| match ev.name {
            "RequestCompleted" => request_completed += 1,
            "HttpRequestWire" => http_request += 1,
            _ => {}
        })
        .map_err(|e| std::io::Error::other(e.to_string()))?;

    println!("RequestCompleted events: {request_completed}");
    println!("HttpRequestWire events:  {http_request}");
    assert_eq!(request_completed, 10);
    assert_eq!(http_request, 10);
    println!("✓ All custom events recorded successfully");

    // ── Read custom events through TraceReader (requires `analysis` feature) ──
    #[cfg(feature = "analysis")]
    {
        use dial9_tokio_telemetry::analysis_unstable::TraceReader;
        use dial9_tokio_telemetry::telemetry::TelemetryEvent;

        let reader = TraceReader::new(sealed.to_str().unwrap())?;

        let custom_events: Vec<&TelemetryEvent> = reader
            .all_events
            .iter()
            .filter(|e| matches!(e, TelemetryEvent::Custom { .. }))
            .collect();

        println!(
            "\n=== TraceReader: {} custom events ===",
            custom_events.len()
        );
        assert_eq!(custom_events.len(), 20); // 10 RequestCompleted + 10 HttpRequestWire

        for event in &custom_events {
            if let TelemetryEvent::Custom { name, fields, .. } = event {
                print!("  {name}:");
                for (fname, fval) in fields {
                    print!(" {fname}={fval:?}");
                }
                println!();
            }
        }
        println!("✓ TraceReader custom event round-trip verified");
    }

    Ok(())
}
