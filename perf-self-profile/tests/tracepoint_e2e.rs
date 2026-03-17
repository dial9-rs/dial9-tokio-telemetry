use dial9_perf_self_profile::tracepoint::TracepointDef;
use dial9_perf_self_profile::{PerfSampler, SamplerConfig};
use dial9_trace_format::decoder::{DecodedFrame, Decoder};
use dial9_trace_format::encoder::Encoder;
use dial9_trace_format::types::FieldValue;

/// End-to-end test: subscribe to sched_switch tracepoint, capture real kernel
/// events, parse raw sample data through TracepointDef, encode into
/// dial9-trace-format, decode, and verify fields.
#[test]
fn tracepoint_sched_switch_e2e() {
    // 1. Parse the kernel format file (includes tracepoint ID)
    let tp = match TracepointDef::from_event("sched", "sched_switch") {
        Ok(tp) => tp,
        Err(e) => {
            eprintln!("skipping: cannot read sched_switch format: {}", e);
            return;
        }
    };
    eprintln!(
        "parsed sched_switch: {} fields, id={}",
        tp.fields.len(),
        tp.id
    );

    // 2. Open perf event using the TracepointDef's event source
    let mut sampler = match PerfSampler::start(SamplerConfig {
        frequency_hz: 1,
        event_source: tp.event_source(),
        include_kernel: false,
    }) {
        Ok(s) => s,
        Err(e) => {
            eprintln!("skipping: perf_event_open failed: {}", e);
            return;
        }
    };

    // 3. Generate context switches
    for _ in 0..5 {
        std::thread::sleep(std::time::Duration::from_millis(10));
    }

    // 4. Collect samples
    sampler.disable();
    let samples = sampler.drain_samples();
    eprintln!("collected {} samples", samples.len());
    assert!(
        !samples.is_empty(),
        "expected at least one sched_switch sample"
    );

    // 5. Register schema + encode raw samples into trace-format
    let mut encoder = Encoder::new();
    let schema = tp.register(&mut encoder).expect("register schema");

    let mut encoded_count = 0;
    for sample in &samples {
        if let Some(raw) = &sample.raw {
            match tp.encode_raw(&mut encoder, &schema, sample.time, raw) {
                Ok(()) => {
                    encoded_count += 1;
                    if encoded_count <= 3 {
                        // Print a few for debugging
                        let extracted = tp.extract_fields(raw).unwrap();
                        let values = tp.to_trace_format_values(&extracted);
                        eprintln!("sample {} (tid={}):", encoded_count, sample.tid);
                        for (field, val) in tp.fields.iter().zip(values.iter()) {
                            eprintln!("  {} = {:?}", field.name, val);
                        }
                    }
                }
                Err(e) => eprintln!("warning: encode_raw failed: {}", e),
            }
        }
    }
    assert!(encoded_count > 0, "expected at least one encoded event");

    // 6. Decode and verify round-trip
    let bytes = encoder.finish();
    let mut decoder = Decoder::new(&bytes).expect("decode");
    let frames = decoder.decode_all();

    let mut schema_count = 0;
    let mut event_count = 0;

    for frame in &frames {
        match frame {
            DecodedFrame::Schema(entry) => {
                assert_eq!(entry.name, "sched_switch");
                assert!(entry.has_timestamp);
                assert_eq!(entry.fields.len(), tp.fields.len());
                schema_count += 1;
            }
            DecodedFrame::Event {
                timestamp_ns,
                values,
                ..
            } => {
                assert!(timestamp_ns.is_some());
                assert_eq!(values.len(), tp.fields.len());
                if values
                    .iter()
                    .any(|v| matches!(v, FieldValue::String(s) if !s.is_empty()))
                {
                    event_count += 1;
                }
            }
            _ => {}
        }
    }

    assert_eq!(schema_count, 1);
    assert!(
        event_count > 0,
        "expected decoded events with non-empty comm strings"
    );
    eprintln!(
        "SUCCESS: {} events round-tripped ({} bytes)",
        event_count,
        bytes.len()
    );
}
