#![no_main]
use dial9_trace_format::decoder::{DecodedFrame, Decoder};
use dial9_trace_format::encoder::Encoder;
use dial9_trace_format::schema::FieldDef;
use dial9_trace_format::types::{FieldType, FieldValue};
use libfuzzer_sys::fuzz_target;

// Encode fuzz-derived events via manual schema, then decode and verify round-trip.
fuzz_target!(|data: &[u8]| {
    if data.len() < 2 {
        return;
    }
    let n_events = data[0] as usize % 16 + 1;
    let seed = data[1];

    struct Ev;
    let mut enc = Encoder::new();
    enc.register_schema_for::<Ev>(
        "FuzzEvent",
        vec![
            FieldDef {
                name: "a".into(),
                field_type: FieldType::Varint,
            },
            FieldDef {
                name: "b".into(),
                field_type: FieldType::Varint,
            },
        ],
    );

    for i in 0..n_events {
        let val = (seed as u64).wrapping_mul(i as u64 + 1);
        enc.write_event_for::<Ev>(&[FieldValue::Varint(val), FieldValue::Varint(i as u64)]);
    }

    let bytes = enc.finish();
    let mut dec = Decoder::new(&bytes).unwrap();
    let frames = dec.decode_all();
    let event_count = frames
        .iter()
        .filter(|f| matches!(f, DecodedFrame::Event { .. }))
        .count();
    assert_eq!(event_count, n_events);
});
