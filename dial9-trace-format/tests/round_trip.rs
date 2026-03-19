use dial9_trace_format::codec::{self, WireTypeId};
use dial9_trace_format::decoder::{DecodedFrame, Decoder};
use dial9_trace_format::encoder::Encoder;
use dial9_trace_format::schema::FieldDef;
use dial9_trace_format::types::{FieldType, FieldValue};

#[test]
fn full_round_trip() {
    let mut enc = Encoder::new();

    let poll_id = enc
        .register_schema(
            "PollStart",
            vec![
                FieldDef {
                    name: "worker".into(),
                    field_type: FieldType::Varint,
                },
                FieldDef {
                    name: "task_id".into(),
                    field_type: FieldType::Varint,
                },
            ],
        )
        .unwrap();
    let cpu_id = enc
        .register_schema(
            "CpuSample",
            vec![
                FieldDef {
                    name: "thread_name".into(),
                    field_type: FieldType::PooledString,
                },
                FieldDef {
                    name: "frames".into(),
                    field_type: FieldType::StackFrames,
                },
            ],
        )
        .unwrap();

    let thread_id = enc.intern_string("worker-0").unwrap();

    enc.write_event(
        &poll_id,
        &[
            FieldValue::Varint(1_000_000),
            FieldValue::Varint(0),
            FieldValue::Varint(42),
        ],
    )
    .unwrap();

    let frames = vec![0x5555_5555_1234u64, 0x5555_5555_0a00, 0x5555_5555_0800];
    enc.write_event(
        &cpu_id,
        &[
            FieldValue::Varint(1_000_100),
            FieldValue::PooledString(thread_id),
            FieldValue::StackFrames(frames.clone()),
        ],
    )
    .unwrap();

    let sym_name_id = enc.intern_string("my_function").unwrap();

    let mut data = enc.finish();

    // Append symbol table as untimestamped schema-based events via low-level codec.
    let sym_tid = WireTypeId(100);
    codec::encode_schema(
        sym_tid,
        &dial9_trace_format::schema::SchemaEntry {
            name: "SymbolTableEntry".into(),
            has_timestamp: false,
            fields: vec![
                FieldDef {
                    name: "base_addr".into(),
                    field_type: FieldType::Varint,
                },
                FieldDef {
                    name: "size".into(),
                    field_type: FieldType::Varint,
                },
                FieldDef {
                    name: "symbol_name".into(),
                    field_type: FieldType::PooledString,
                },
            ],
        },
        &mut data,
    )
    .unwrap();
    codec::encode_event(
        sym_tid,
        None,
        &[
            FieldValue::Varint(0x5555_5555_0000),
            FieldValue::Varint(0x2000),
            FieldValue::PooledString(sym_name_id),
        ],
        &mut data,
    )
    .unwrap();

    let mut dec = Decoder::new(&data).unwrap();
    assert_eq!(dec.version(), 1);

    let decoded = dec.decode_all();

    // 2 schemas(PollStart,CpuSample) + 1 pool("worker-0") + 1 poll event + 1 cpu sample
    // + 1 pool("my_function") + 1 schema(SymbolTableEntry) + 1 symbol event = 8
    assert_eq!(decoded.len(), 8, "got: {decoded:#?}");

    assert!(matches!(&decoded[0], DecodedFrame::Schema(s) if s.name == "PollStart"));
    assert!(matches!(&decoded[1], DecodedFrame::Schema(s) if s.name == "CpuSample"));

    assert_eq!(dec.string_pool().get(thread_id), Some("worker-0"));
    assert_eq!(dec.string_pool().get(sym_name_id), Some("my_function"));

    // Verify poll event
    if let DecodedFrame::Event { values, .. } = &decoded[3] {
        assert_eq!(*values, vec![FieldValue::Varint(0), FieldValue::Varint(42)]);
    } else {
        panic!("expected event frame");
    }

    // Verify cpu sample with stack frames
    if let DecodedFrame::Event { values, .. } = &decoded[4] {
        assert_eq!(values[0], FieldValue::PooledString(thread_id));
        assert_eq!(values[1], FieldValue::StackFrames(frames));
    } else {
        panic!("expected event frame");
    }

    // Verify symbol table event
    assert!(matches!(&decoded[6], DecodedFrame::Schema(s) if s.name == "SymbolTableEntry"));
    if let DecodedFrame::Event { values, .. } = &decoded[7] {
        assert_eq!(values[0], FieldValue::Varint(0x5555_5555_0000));
        assert_eq!(values[1], FieldValue::Varint(0x2000));
        assert_eq!(values[2], FieldValue::PooledString(sym_name_id));
    } else {
        panic!("expected symbol table event");
    }
}

#[test]
fn round_trip_all_field_types() {
    let mut enc = Encoder::new();
    let tid = enc
        .register_schema(
            "AllTypes",
            vec![
                FieldDef {
                    name: "a".into(),
                    field_type: FieldType::Varint,
                },
                FieldDef {
                    name: "b".into(),
                    field_type: FieldType::I64,
                },
                FieldDef {
                    name: "c".into(),
                    field_type: FieldType::F64,
                },
                FieldDef {
                    name: "d".into(),
                    field_type: FieldType::Bool,
                },
                FieldDef {
                    name: "e".into(),
                    field_type: FieldType::String,
                },
                FieldDef {
                    name: "f".into(),
                    field_type: FieldType::Bytes,
                },
                FieldDef {
                    name: "h".into(),
                    field_type: FieldType::PooledString,
                },
                FieldDef {
                    name: "i".into(),
                    field_type: FieldType::StackFrames,
                },
            ],
        )
        .unwrap();

    let pool_id = enc.intern_string("test").unwrap();
    let values = vec![
        FieldValue::Varint(1_000_000), // timestamp
        FieldValue::Varint(u64::MAX),
        FieldValue::I64(i64::MIN),
        FieldValue::F64(std::f64::consts::E),
        FieldValue::Bool(false),
        FieldValue::String("hello".to_string()),
        FieldValue::Bytes(vec![0xDE, 0xAD]),
        FieldValue::PooledString(pool_id),
        FieldValue::StackFrames(vec![0xAAAA, 0xBBBB, 0xCCCC]),
    ];
    enc.write_event(&tid, &values).unwrap();
    let data = enc.finish();

    let mut dec = Decoder::new(&data).unwrap();
    let frames = dec.decode_all();
    let event = frames
        .iter()
        .find(|f| matches!(f, DecodedFrame::Event { .. }))
        .unwrap();
    if let DecodedFrame::Event {
        values: decoded_values,
        ..
    } = event
    {
        // Decoded values don't include the timestamp (it's in the header)
        assert_eq!(decoded_values, &values[1..]);
    }
}
