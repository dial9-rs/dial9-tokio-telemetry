//! End-to-end tests for thread-local encoding: encoder reset, raw-copy
//! concatenation (reset-frame pattern), and fallible iteration.

use dial9_trace_format::decoder::Decoder;
use dial9_trace_format::encoder::Encoder;
use dial9_trace_format::{InternedString, TraceEvent};

#[derive(TraceEvent)]
struct TestEvent {
    #[traceevent(timestamp)]
    timestamp_ns: u64,
    value: u32,
}

#[derive(TraceEvent)]
struct StringEvent {
    #[traceevent(timestamp)]
    timestamp_ns: u64,
    name: InternedString,
}

// ── Encoder reset ───────────────────────────────────────────────────────

#[test]
fn test_reset_to_returns_decodable_bytes() {
    let mut enc = Encoder::new();
    enc.write(&TestEvent {
        timestamp_ns: 1000,
        value: 42,
    })
    .unwrap();
    enc.write(&TestEvent {
        timestamp_ns: 2000,
        value: 99,
    })
    .unwrap();

    let bytes = enc.reset_to_infallible(Vec::new());

    let mut dec = Decoder::new(&bytes).unwrap();
    let mut count = 0;
    dec.for_each_event(|ev| {
        count += 1;
        assert!(ev.timestamp_ns.is_some());
    })
    .unwrap();
    assert_eq!(count, 2);

    enc.write(&TestEvent {
        timestamp_ns: 3000,
        value: 1,
    })
    .unwrap();
    let bytes2 = enc.reset_to_infallible(Vec::new());
    let mut dec2 = Decoder::new(&bytes2).unwrap();
    let mut count2 = 0;
    dec2.for_each_event(|_| count2 += 1).unwrap();
    assert_eq!(count2, 1, "encoder should be ready for new session");
}

#[test]
fn test_reset_convenience_returns_decodable_bytes() {
    let mut enc = Encoder::new();
    enc.write(&TestEvent {
        timestamp_ns: 1000,
        value: 10,
    })
    .unwrap();

    let bytes = enc.reset_to_infallible(Vec::new());
    let mut dec = Decoder::new(&bytes).unwrap();
    let mut count = 0;
    dec.for_each_event(|_| count += 1).unwrap();
    assert_eq!(count, 1);
}

// ── Raw-copy concatenation (reset-frame pattern) ────────────────────────

#[test]
fn test_rawcopy_round_trip_single_batch() {
    let mut enc1 = Encoder::new();
    enc1.write(&TestEvent {
        timestamp_ns: 1000,
        value: 42,
    })
    .unwrap();
    enc1.write(&TestEvent {
        timestamp_ns: 2000,
        value: 99,
    })
    .unwrap();
    let bytes1 = enc1.reset_to_infallible(Vec::new());

    // Simulate raw-copy: just concatenate onto an output stream
    let mut output = Vec::new();
    dial9_trace_format::codec::encode_header(&mut output).unwrap();
    output.extend_from_slice(&bytes1);

    let mut dec = Decoder::new(&output).unwrap();
    let mut events = Vec::new();
    dec.for_each_event(|ev| {
        events.push(ev.timestamp_ns);
    })
    .unwrap();
    assert_eq!(events, vec![Some(1000), Some(2000)]);
}

#[test]
fn test_rawcopy_string_pool_per_batch() {
    let mut enc1 = Encoder::new();
    let s1 = enc1.intern_string("shared").unwrap();
    enc1.write(&StringEvent {
        timestamp_ns: 1000,
        name: s1,
    })
    .unwrap();
    let bytes1 = enc1.reset_to_infallible(Vec::new());

    let mut enc2 = Encoder::new();
    let s2 = enc2.intern_string("shared").unwrap();
    enc2.write(&StringEvent {
        timestamp_ns: 2000,
        name: s2,
    })
    .unwrap();
    let bytes2 = enc2.reset_to_infallible(Vec::new());

    // Concatenate both batches (each has its own header)
    let mut output = Vec::new();
    dial9_trace_format::codec::encode_header(&mut output).unwrap();
    output.extend_from_slice(&bytes1);
    output.extend_from_slice(&bytes2);

    let mut dec = Decoder::new(&output).unwrap();
    let mut count = 0;
    dec.for_each_event(|_| count += 1).unwrap();
    assert_eq!(count, 2);
    // Each batch has its own pool, so after reset the pool only has the last batch's entries
    // (the decoder resets on each mid-stream header)
}

#[test]
fn test_rawcopy_timestamp_preserved() {
    let mut enc1 = Encoder::new();
    enc1.write(&TestEvent {
        timestamp_ns: 5000,
        value: 1,
    })
    .unwrap();
    let bytes1 = enc1.reset_to_infallible(Vec::new());

    let mut enc2 = Encoder::new();
    enc2.write(&TestEvent {
        timestamp_ns: 10000,
        value: 2,
    })
    .unwrap();
    let bytes2 = enc2.reset_to_infallible(Vec::new());

    let mut output = Vec::new();
    dial9_trace_format::codec::encode_header(&mut output).unwrap();
    output.extend_from_slice(&bytes1);
    output.extend_from_slice(&bytes2);

    let mut dec = Decoder::new(&output).unwrap();
    let mut timestamps = Vec::new();
    dec.for_each_event(|ev| {
        timestamps.push(ev.timestamp_ns);
    })
    .unwrap();
    assert_eq!(timestamps, vec![Some(5000), Some(10000)]);
}

#[test]
fn test_rawcopy_different_schemas() {
    let mut enc1 = Encoder::new();
    enc1.write(&TestEvent {
        timestamp_ns: 1000,
        value: 42,
    })
    .unwrap();
    let bytes1 = enc1.finish();

    let mut enc2 = Encoder::new();
    let name = enc2.intern_string("hello").unwrap();
    enc2.write(&StringEvent {
        timestamp_ns: 2000,
        name,
    })
    .unwrap();
    let bytes2 = enc2.finish();

    // Concatenate: different schemas at type_id 0 in each batch
    let mut combined = bytes1;
    combined.extend_from_slice(&bytes2);

    let mut dec = Decoder::new(&combined).unwrap();
    let mut names = Vec::new();
    dec.for_each_event(|ev| {
        names.push(ev.name.to_string());
    })
    .unwrap();
    assert_eq!(names, vec!["TestEvent", "StringEvent"]);
}

#[test]
fn test_rawcopy_empty_batch() {
    let mut enc1 = Encoder::new();
    let bytes1 = enc1.reset_to_infallible(Vec::new());

    let mut output = Vec::new();
    dial9_trace_format::codec::encode_header(&mut output).unwrap();
    output.extend_from_slice(&bytes1);

    let mut dec = Decoder::new(&output).unwrap();
    let mut count = 0;
    dec.for_each_event(|_| count += 1).unwrap();
    assert_eq!(count, 0);
}

// ── Fallible iteration ──────────────────────────────────────────────────

#[test]
fn test_try_for_each_event_propagates_error() {
    let mut enc = Encoder::new();
    enc.write(&TestEvent {
        timestamp_ns: 1000,
        value: 1,
    })
    .unwrap();
    enc.write(&TestEvent {
        timestamp_ns: 2000,
        value: 2,
    })
    .unwrap();
    enc.write(&TestEvent {
        timestamp_ns: 3000,
        value: 3,
    })
    .unwrap();
    let bytes = enc.reset_to_infallible(Vec::new());

    let mut dec = Decoder::new(&bytes).unwrap();
    let mut processed = 0;
    let result = dec.try_for_each_event(|_ev| {
        processed += 1;
        if processed == 2 { Err("stop") } else { Ok(()) }
    });

    assert!(result.is_err());
    assert_eq!(processed, 2);
}

#[test]
fn test_try_for_each_event_success() {
    let mut enc = Encoder::new();
    enc.write(&TestEvent {
        timestamp_ns: 1000,
        value: 1,
    })
    .unwrap();
    enc.write(&TestEvent {
        timestamp_ns: 2000,
        value: 2,
    })
    .unwrap();
    let bytes = enc.reset_to_infallible(Vec::new());

    let mut dec = Decoder::new(&bytes).unwrap();
    let mut count = 0;
    let result = dec.try_for_each_event(|_ev| {
        count += 1;
        Ok::<(), ()>(())
    });

    assert!(result.is_ok());
    assert_eq!(count, 2);
}
