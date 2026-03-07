use dial9_trace_format::TraceEvent;
use dial9_trace_format::decoder::{DecodedFrame, Decoder};
use dial9_trace_format::encoder::Encoder;
use dial9_trace_format::types::{FieldType, FieldValue};

#[derive(TraceEvent)]
struct SpanEvent {
    timestamp_ns: u64,
    parent_id: u64,
    name: String,
    duration_ns: u64,
}

#[test]
fn derive_event_name() {
    assert_eq!(SpanEvent::event_name(), "SpanEvent");
}

#[test]
fn derive_field_defs() {
    let defs = SpanEvent::field_defs();
    assert_eq!(defs.len(), 4);
    assert_eq!(defs[0].name, "timestamp_ns");
    assert_eq!(defs[0].field_type, FieldType::Varint);
    assert_eq!(defs[1].name, "parent_id");
    assert_eq!(defs[1].field_type, FieldType::Varint);
    assert_eq!(defs[2].name, "name");
    assert_eq!(defs[2].field_type, FieldType::String);
    assert_eq!(defs[3].name, "duration_ns");
    assert_eq!(defs[3].field_type, FieldType::Varint);
}

#[test]
fn derive_to_values() {
    let ev = SpanEvent {
        timestamp_ns: 1_000_000,
        parent_id: 42,
        name: "my_span".to_string(),
        duration_ns: 500,
    };
    let mut enc = Encoder::new();
    enc.write(&ev);
    let data = enc.finish();
    let mut dec = Decoder::new(&data).unwrap();
    let frames = dec.decode_all();
    if let DecodedFrame::Event { values, .. } = &frames[1] {
        assert_eq!(*values, vec![
            FieldValue::Varint(1_000_000),
            FieldValue::Varint(42),
            FieldValue::String(b"my_span".to_vec()),
            FieldValue::Varint(500),
        ]);
    } else {
        panic!("expected event");
    }
}

#[derive(TraceEvent)]
struct AllSupportedTypes {
    a: u64,
    b: i64,
    c: f64,
    d: bool,
    e: String,
    f: Vec<u8>,
}

#[test]
fn derive_all_types_round_trip() {
    let ev = AllSupportedTypes {
        a: 1, b: -2, c: 3.14, d: true,
        e: "hello".to_string(), f: vec![0xAB],
    };

    let mut enc = Encoder::new();
    enc.write(&ev);
    let data = enc.finish();

    let mut dec = Decoder::new(&data).unwrap();
    let frames = dec.decode_all();
    let event = frames.iter().find(|f| matches!(f, DecodedFrame::Event { .. })).unwrap();
    if let DecodedFrame::Event { values, .. } = event {
        assert_eq!(values.len(), 6);
    } else {
        panic!("expected event");
    }
}

#[derive(TraceEvent)]
struct SmallTypes {
    a: u8,
    b: u16,
    c: u32,
    d: u64,
}

#[test]
fn derive_small_int_types_use_varint() {
    let defs = SmallTypes::field_defs();
    for def in &defs {
        assert_eq!(def.field_type, FieldType::Varint, "field {} should be Varint", def.name);
    }
    let ev = SmallTypes { a: 255, b: 1000, c: 100_000, d: u64::MAX };
    let mut enc = Encoder::new();
    enc.write(&ev);
    let data = enc.finish();
    let mut dec = Decoder::new(&data).unwrap();
    let frames = dec.decode_all();
    if let DecodedFrame::Event { values, .. } = &frames[1] {
        assert_eq!(values[0], FieldValue::Varint(255));
        assert_eq!(values[1], FieldValue::Varint(1000));
        assert_eq!(values[2], FieldValue::Varint(100_000));
        assert_eq!(values[3], FieldValue::Varint(u64::MAX));
    } else {
        panic!("expected event");
    }
}

#[test]
fn encoder_write_auto_registers() {
    #[derive(TraceEvent)]
    struct MyEvent { value: u64 }

    let mut enc = Encoder::new();
    enc.write(&MyEvent { value: 42 });
    enc.write(&MyEvent { value: 99 });
    let data = enc.finish();

    let mut dec = Decoder::new(&data).unwrap();
    let frames = dec.decode_all();
    assert_eq!(frames.len(), 3);
    assert!(matches!(&frames[0], DecodedFrame::Schema(s) if s.name == "MyEvent"));
    assert!(matches!(&frames[1], DecodedFrame::Event { .. }));
    assert!(matches!(&frames[2], DecodedFrame::Event { .. }));
}

#[test]
fn derive_with_stack_frames() {
    use dial9_trace_format::{StackFrames, TraceEvent};
    use dial9_trace_format::types::FieldValue;

    #[derive(TraceEvent)]
    struct CpuSample {
        timestamp_ns: u64,
        worker_id: u64,
        tid: u32,
        source: u8,
        frames: StackFrames,
    }

    let ev = CpuSample {
        timestamp_ns: 1_000_000,
        worker_id: 0,
        tid: 12345,
        source: 0,
        frames: StackFrames(vec![0x5555_5555_1234, 0x5555_5555_0a00]),
    };

    let mut enc = Encoder::new();
    enc.write(&ev);
    let data = enc.finish();

    let mut dec = Decoder::new(&data).unwrap();
    let frames = dec.decode_all();
    assert_eq!(frames.len(), 2);
    if let DecodedFrame::Event { values, .. } = &frames[1] {
        assert_eq!(values[4], FieldValue::StackFrames(vec![0x5555_5555_1234, 0x5555_5555_0a00]));
    } else {
        panic!("expected event");
    }
}

#[test]
fn derive_with_interned_string() {
    use dial9_trace_format::{InternedString, TraceEvent};
    use dial9_trace_format::types::{FieldType, FieldValue};

    #[derive(TraceEvent)]
    struct LogEvent {
        timestamp_ns: u64,
        source: InternedString,
    }

    assert_eq!(LogEvent::field_defs()[1].field_type, FieldType::PooledString);

    let mut enc = Encoder::new();
    let src = enc.intern_string("main");
    enc.write(&LogEvent { timestamp_ns: 42, source: src });
    let data = enc.finish();

    let mut dec = Decoder::new(&data).unwrap();
    let frames = dec.decode_all();
    // schema + string_pool + event = 3
    assert_eq!(frames.len(), 3);
    if let DecodedFrame::Event { values, .. } = &frames[2] {
        assert_eq!(values[1], FieldValue::PooledString(src.0));
    } else {
        panic!("expected event");
    }
    assert_eq!(dec.string_pool.get(&src.0), Some(&"main".to_string()));
}
