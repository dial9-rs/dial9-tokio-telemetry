//! Tests validating SPEC.md edge cases and format limits.

use dial9_trace_format::codec::{
    self, Frame, HEADER_SIZE, MAGIC, PoolEntry, SchemaInfo, SymbolEntry, VERSION, WireTypeId,
};
use dial9_trace_format::decoder::{DecodedFrame, Decoder};
use dial9_trace_format::encoder::Encoder;
use dial9_trace_format::schema::{FieldDef, SchemaEntry};
use dial9_trace_format::types::{FieldType, FieldValue, InternedString};

// --- Header edge cases ---

#[test]
fn header_is_exactly_5_bytes() {
    let mut buf = Vec::new();
    codec::encode_header(&mut buf).unwrap();
    assert_eq!(buf.len(), HEADER_SIZE);
    assert_eq!(&buf[..4], &MAGIC);
    assert_eq!(buf[4], VERSION);
}

#[test]
fn empty_stream_after_header() {
    let data = Encoder::new().finish();
    assert_eq!(data.len(), 5);
    let mut dec = Decoder::new(&data).unwrap();
    assert!(dec.next_frame().unwrap().is_none());
}

// --- Schema edge cases (codec-level, not encoder) ---

#[test]
fn schema_max_type_id() {
    let type_id = WireTypeId(u16::MAX);
    let entry = SchemaEntry {
        name: "Max".into(),
        has_timestamp: false,
        fields: vec![FieldDef {
            name: "v".into(),
            field_type: FieldType::Varint,
        }],
    };
    let mut buf = Vec::new();
    codec::encode_schema(type_id, &entry, &mut buf).unwrap();
    let types = vec![FieldType::Varint];
    let (frame, _) = codec::decode_frame(&buf, |_| None, 0).unwrap();
    assert_eq!(frame, Frame::Schema { type_id, entry });

    // Encode and decode an event with that type_id
    let mut event_buf = Vec::new();
    codec::encode_event(type_id, None, &[FieldValue::Varint(1)], &mut event_buf).unwrap();
    let lookup = |id: WireTypeId| -> Option<SchemaInfo<'_>> {
        if id == type_id {
            Some(SchemaInfo {
                field_types: &types,
                has_timestamp: false,
            })
        } else {
            None
        }
    };
    let (frame, _) = codec::decode_frame(&event_buf, lookup, 0).unwrap();
    assert!(matches!(frame, Frame::Event { type_id: tid, .. } if tid == type_id));
}

#[test]
fn schema_empty_name() {
    let type_id = WireTypeId(0);
    let entry = SchemaEntry {
        name: "".into(),
        has_timestamp: false,
        fields: vec![],
    };
    let mut buf = Vec::new();
    codec::encode_schema(type_id, &entry, &mut buf).unwrap();
    let (frame, _) = codec::decode_frame(&buf, |_| None, 0).unwrap();
    assert_eq!(frame, Frame::Schema { type_id, entry });
}

#[test]
fn schema_many_fields() {
    let type_id = WireTypeId(1);
    let fields: Vec<FieldDef> = (0..256)
        .map(|i| FieldDef {
            name: format!("f{i}"),
            field_type: FieldType::Varint,
        })
        .collect();
    let entry = SchemaEntry {
        name: "Wide".into(),
        has_timestamp: false,
        fields: fields.clone(),
    };
    let mut buf = Vec::new();
    codec::encode_schema(type_id, &entry, &mut buf).unwrap();
    let (frame, _) = codec::decode_frame(&buf, |_| None, 0).unwrap();
    assert_eq!(frame, Frame::Schema { type_id, entry });
}

// --- Field type edge cases ---

#[test]
fn u64_boundary_values() {
    for val in [0u64, 1, u64::MAX] {
        let v = FieldValue::Varint(val);
        let mut buf = Vec::new();
        v.encode(&mut buf).unwrap();
        let (decoded, _) = FieldValue::decode(FieldType::Varint, &buf).unwrap();
        assert_eq!(decoded, v);
    }
}

#[test]
fn i64_boundary_values() {
    for val in [i64::MIN, -1, 0, 1, i64::MAX] {
        let v = FieldValue::I64(val);
        let mut buf = Vec::new();
        v.encode(&mut buf).unwrap();
        let (decoded, _) = FieldValue::decode(FieldType::I64, &buf).unwrap();
        assert_eq!(decoded, v);
    }
}

#[test]
fn f64_special_values() {
    for val in [0.0, -0.0, f64::INFINITY, f64::NEG_INFINITY] {
        let v = FieldValue::F64(val);
        let mut buf = Vec::new();
        v.encode(&mut buf).unwrap();
        let (decoded, _) = FieldValue::decode(FieldType::F64, &buf).unwrap();
        assert_eq!(decoded, v);
    }
    let v = FieldValue::F64(f64::NAN);
    let mut buf = Vec::new();
    v.encode(&mut buf).unwrap();
    let (decoded, _) = FieldValue::decode(FieldType::F64, &buf).unwrap();
    if let FieldValue::F64(d) = decoded {
        assert!(d.is_nan());
    }
}

#[test]
fn empty_string_field() {
    let v = FieldValue::String(String::new());
    let mut buf = Vec::new();
    v.encode(&mut buf).unwrap();
    assert_eq!(buf.len(), 4);
    let (decoded, rest) = FieldValue::decode(FieldType::String, &buf).unwrap();
    assert!(rest.is_empty());
    assert_eq!(decoded, v);
}

#[test]
fn empty_bytes_field() {
    let v = FieldValue::Bytes(vec![]);
    let mut buf = Vec::new();
    v.encode(&mut buf).unwrap();
    let (decoded, _) = FieldValue::decode(FieldType::Bytes, &buf).unwrap();
    assert_eq!(decoded, v);
}

#[test]
fn empty_stack_frames() {
    let v = FieldValue::StackFrames(vec![]);
    let mut buf = Vec::new();
    v.encode(&mut buf).unwrap();
    assert_eq!(buf.len(), 4);
    let (decoded, _) = FieldValue::decode(FieldType::StackFrames, &buf).unwrap();
    assert_eq!(decoded, v);
}

#[test]
fn varint_zero() {
    let v = FieldValue::Varint(0);
    let mut buf = Vec::new();
    v.encode(&mut buf).unwrap();
    assert_eq!(buf.len(), 1);
    assert_eq!(buf[0], 0x00);
    let (decoded, rest) = FieldValue::decode(FieldType::Varint, &buf).unwrap();
    assert!(rest.is_empty());
    assert_eq!(decoded, v);
}

#[test]
fn varint_max() {
    let v = FieldValue::Varint(u64::MAX);
    let mut buf = Vec::new();
    v.encode(&mut buf).unwrap();
    assert_eq!(buf.len(), 10);
    let (decoded, rest) = FieldValue::decode(FieldType::Varint, &buf).unwrap();
    assert!(rest.is_empty());
    assert_eq!(decoded, v);
}

#[test]
fn varint_leb128_boundary_values() {
    let cases = [(127u64, 1), (128, 2), (16383, 2), (16384, 3)];
    for (val, expected_bytes) in cases {
        let v = FieldValue::Varint(val);
        let mut buf = Vec::new();
        v.encode(&mut buf).unwrap();
        assert_eq!(
            buf.len(),
            expected_bytes,
            "Varint({val}) should be {expected_bytes} bytes"
        );
        let (decoded, _) = FieldValue::decode(FieldType::Varint, &buf).unwrap();
        assert_eq!(decoded, v);
    }
}

// --- StackFrames delta encoding ---

#[test]
fn stack_frames_single_address() {
    let v = FieldValue::StackFrames(vec![0x7fff_ffff_ffff]);
    let mut buf = Vec::new();
    v.encode(&mut buf).unwrap();
    let (decoded, _) = FieldValue::decode(FieldType::StackFrames, &buf).unwrap();
    assert_eq!(decoded, v);
}

#[test]
fn stack_frames_identical_addresses() {
    let addrs = vec![0x1000u64; 5];
    let v = FieldValue::StackFrames(addrs);
    let mut buf = Vec::new();
    v.encode(&mut buf).unwrap();
    assert_eq!(buf.len(), 4 + 5 * 8); // count + 5 raw u64s
    let (decoded, _) = FieldValue::decode(FieldType::StackFrames, &buf).unwrap();
    assert_eq!(decoded, v);
}

#[test]
fn stack_frames_descending_addresses() {
    let addrs = vec![0x5555_5555_5000u64, 0x5555_5555_4000, 0x5555_5555_3000];
    let v = FieldValue::StackFrames(addrs);
    let mut buf = Vec::new();
    v.encode(&mut buf).unwrap();
    let (decoded, _) = FieldValue::decode(FieldType::StackFrames, &buf).unwrap();
    assert_eq!(decoded, v);
}

// --- String pool edge cases ---

#[test]
fn string_pool_empty_string() {
    let entries = vec![PoolEntry {
        pool_id: 0,
        data: vec![],
    }];
    let mut buf = Vec::new();
    codec::encode_string_pool(&entries, &mut buf).unwrap();
    let (frame, _) = codec::decode_frame(&buf, |_| None, 0).unwrap();
    assert_eq!(frame, Frame::StringPool(entries));
}

#[test]
fn string_pool_many_entries() {
    let entries: Vec<PoolEntry> = (0..100)
        .map(|i| PoolEntry {
            pool_id: i,
            data: format!("str_{i}").into_bytes(),
        })
        .collect();
    let mut buf = Vec::new();
    codec::encode_string_pool(&entries, &mut buf).unwrap();
    let (frame, _) = codec::decode_frame(&buf, |_| None, 0).unwrap();
    assert_eq!(frame, Frame::StringPool(entries));
}

// --- Symbol table edge cases ---

#[test]
fn symbol_table_max_address() {
    let entries = vec![SymbolEntry {
        base_addr: u64::MAX,
        size: u32::MAX,
        symbol_id: InternedString::from_raw(u32::MAX),
    }];
    let mut buf = Vec::new();
    codec::encode_symbol_table(&entries, &mut buf).unwrap();
    let (frame, _) = codec::decode_frame(&buf, |_| None, 0).unwrap();
    assert_eq!(frame, Frame::SymbolTable(entries));
}

// --- Multi-frame ordering ---

#[test]
fn multiple_schemas_then_events() {
    let mut enc = Encoder::new();
    let fields = vec![FieldDef {
        name: "v".into(),
        field_type: FieldType::Varint,
    }];
    let schemas: Vec<_> = (0..5)
        .map(|i| {
            enc.register_schema(&format!("Ev{i}"), fields.clone())
                .unwrap()
        })
        .collect();
    for (i, s) in schemas.iter().enumerate() {
        enc.write_event(
            s,
            &[
                FieldValue::Varint(i as u64 * 1000),
                FieldValue::Varint(i as u64),
            ],
        )
        .unwrap();
    }
    let data = enc.finish();
    let mut dec = Decoder::new(&data).unwrap();
    let frames = dec.decode_all();
    assert_eq!(frames.len(), 10);
    for f in &frames[..5] {
        assert!(matches!(f, DecodedFrame::Schema(_)));
    }
    for f in &frames[5..] {
        assert!(matches!(f, DecodedFrame::Event { .. }));
    }
}

#[test]
fn interleaved_pool_and_events() {
    let mut enc = Encoder::new();
    let schema = enc
        .register_schema(
            "Ev",
            vec![FieldDef {
                name: "s".into(),
                field_type: FieldType::PooledString,
            }],
        )
        .unwrap();
    let id0 = enc.intern_string("first").unwrap();
    enc.write_event(
        &schema,
        &[FieldValue::Varint(1_000), FieldValue::PooledString(id0)],
    )
    .unwrap();
    let id1 = enc.intern_string("second").unwrap();
    enc.write_event(
        &schema,
        &[FieldValue::Varint(2_000), FieldValue::PooledString(id1)],
    )
    .unwrap();
    let data = enc.finish();

    let mut dec = Decoder::new(&data).unwrap();
    let frames = dec.decode_all();
    assert_eq!(frames.len(), 5);
    assert_eq!(dec.string_pool().get(id0), Some("first"));
    assert_eq!(dec.string_pool().get(id1), Some("second"));
}

// --- Field type tag exhaustiveness ---

#[test]
fn all_field_type_tags_valid() {
    for tag in [1, 2, 3, 4, 5, 7, 8, 9, 10, 11, 12, 13u8] {
        assert!(
            FieldType::from_tag(tag).is_some(),
            "tag {tag} should be valid"
        );
    }
}

#[test]
fn field_type_tag_0_invalid() {
    assert!(FieldType::from_tag(0).is_none());
}

#[test]
fn field_type_tag_14_invalid() {
    assert!(FieldType::from_tag(14).is_none());
}

#[test]
fn field_type_tag_255_invalid() {
    assert!(FieldType::from_tag(255).is_none());
}

// --- Truncated data handling ---

#[test]
fn truncated_header() {
    assert!(Decoder::new(&[0x54, 0x52, 0x43]).is_none());
}

#[test]
fn truncated_event_frame() {
    let types = vec![FieldType::Varint];
    let data = [0x02, 0x01];
    let result = codec::decode_frame(
        &data,
        |_| {
            Some(SchemaInfo {
                field_types: &types,
                has_timestamp: false,
            })
        },
        0,
    );
    assert!(result.is_none());
}

#[test]
fn truncated_schema_frame() {
    let data = [0x01, 0x00, 0x00];
    let result = codec::decode_frame(&data, |_: WireTypeId| None, 0);
    assert!(result.is_none());
}
