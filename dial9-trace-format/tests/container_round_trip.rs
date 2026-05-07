//! Round-trip tests for DynamicList and DynamicMap field types.

use dial9_trace_format::decoder::{DecodedFrame, Decoder};
use dial9_trace_format::encoder::Encoder;
use dial9_trace_format::schema::FieldDef;
use dial9_trace_format::types::{FieldType, FieldValue};

#[test]
fn dynamic_list_round_trip() {
    let mut enc = Encoder::new();
    let schema = enc
        .register_schema(
            "ListEvent",
            vec![FieldDef::new("items", FieldType::DynamicList)],
        )
        .unwrap();

    let items = vec![
        FieldValue::String("hello".into()),
        FieldValue::String("world".into()),
        FieldValue::Varint(42),
    ];
    enc.write_event(
        &schema,
        &[FieldValue::Varint(1000), FieldValue::List(items.clone())],
    )
    .unwrap();

    let data = enc.finish();
    let mut dec = Decoder::new(&data).unwrap();
    let events: Vec<_> = dec
        .decode_all()
        .into_iter()
        .filter_map(|f| match f {
            DecodedFrame::Event { values, .. } => Some(values),
            _ => None,
        })
        .collect();

    assert_eq!(events.len(), 1);
    assert_eq!(events[0][0], FieldValue::List(items));
}

#[test]
fn dynamic_map_round_trip() {
    let mut enc = Encoder::new();
    let schema = enc
        .register_schema(
            "MapEvent",
            vec![FieldDef::new("props", FieldType::DynamicMap)],
        )
        .unwrap();

    let pairs = vec![
        (FieldValue::String("count".into()), FieldValue::Varint(10)),
        (
            FieldValue::String("name".into()),
            FieldValue::String("test".into()),
        ),
    ];
    enc.write_event(
        &schema,
        &[FieldValue::Varint(2000), FieldValue::Map(pairs.clone())],
    )
    .unwrap();

    let data = enc.finish();
    let mut dec = Decoder::new(&data).unwrap();
    let events: Vec<_> = dec
        .decode_all()
        .into_iter()
        .filter_map(|f| match f {
            DecodedFrame::Event { values, .. } => Some(values),
            _ => None,
        })
        .collect();

    assert_eq!(events.len(), 1);
    assert_eq!(events[0][0], FieldValue::Map(pairs));
}

#[test]
fn optional_dynamic_list_round_trip() {
    let mut enc = Encoder::new();
    let schema = enc
        .register_schema(
            "OptListEvent",
            vec![FieldDef::new("tags", FieldType::OptionalDynamicList)],
        )
        .unwrap();

    // Present
    let items = vec![FieldValue::Varint(1), FieldValue::Varint(2)];
    enc.write_event(
        &schema,
        &[FieldValue::Varint(3000), FieldValue::List(items.clone())],
    )
    .unwrap();
    // Absent
    enc.write_event(&schema, &[FieldValue::Varint(3001), FieldValue::None])
        .unwrap();

    let data = enc.finish();
    let mut dec = Decoder::new(&data).unwrap();
    let events: Vec<_> = dec
        .decode_all()
        .into_iter()
        .filter_map(|f| match f {
            DecodedFrame::Event { values, .. } => Some(values),
            _ => None,
        })
        .collect();

    assert_eq!(events.len(), 2);
    assert_eq!(events[0][0], FieldValue::List(items));
    assert_eq!(events[1][0], FieldValue::None);
}

#[test]
fn heterogeneous_list_round_trip() {
    let mut enc = Encoder::new();
    let schema = enc
        .register_schema(
            "MixedList",
            vec![FieldDef::new("data", FieldType::DynamicList)],
        )
        .unwrap();

    let items = vec![
        FieldValue::String("hello".into()),
        FieldValue::Varint(42),
        FieldValue::Bool(true),
        FieldValue::F64(1.5),
        FieldValue::I64(-1),
    ];
    enc.write_event(
        &schema,
        &[FieldValue::Varint(4000), FieldValue::List(items.clone())],
    )
    .unwrap();

    let data = enc.finish();
    let mut dec = Decoder::new(&data).unwrap();
    let events: Vec<_> = dec
        .decode_all()
        .into_iter()
        .filter_map(|f| match f {
            DecodedFrame::Event { values, .. } => Some(values),
            _ => None,
        })
        .collect();

    assert_eq!(events.len(), 1);
    assert_eq!(events[0][0], FieldValue::List(items));
}

#[test]
fn nested_list_in_map_round_trip() {
    let mut enc = Encoder::new();
    let schema = enc
        .register_schema(
            "NestedEvent",
            vec![FieldDef::new("nested", FieldType::DynamicMap)],
        )
        .unwrap();

    let inner_list = FieldValue::List(vec![FieldValue::Varint(1), FieldValue::Varint(2)]);
    let pairs = vec![(FieldValue::String("nums".into()), inner_list.clone())];
    enc.write_event(
        &schema,
        &[FieldValue::Varint(5000), FieldValue::Map(pairs.clone())],
    )
    .unwrap();

    let data = enc.finish();
    let mut dec = Decoder::new(&data).unwrap();
    let events: Vec<_> = dec
        .decode_all()
        .into_iter()
        .filter_map(|f| match f {
            DecodedFrame::Event { values, .. } => Some(values),
            _ => None,
        })
        .collect();

    assert_eq!(events.len(), 1);
    assert_eq!(events[0][0], FieldValue::Map(pairs));
}

#[test]
fn empty_list_and_map_round_trip() {
    let mut enc = Encoder::new();
    let schema = enc
        .register_schema(
            "EmptyContainers",
            vec![
                FieldDef::new("list", FieldType::DynamicList),
                FieldDef::new("map", FieldType::DynamicMap),
            ],
        )
        .unwrap();

    enc.write_event(
        &schema,
        &[
            FieldValue::Varint(6000),
            FieldValue::List(vec![]),
            FieldValue::Map(vec![]),
        ],
    )
    .unwrap();

    let data = enc.finish();
    let mut dec = Decoder::new(&data).unwrap();
    let events: Vec<_> = dec
        .decode_all()
        .into_iter()
        .filter_map(|f| match f {
            DecodedFrame::Event { values, .. } => Some(values),
            _ => None,
        })
        .collect();

    assert_eq!(events.len(), 1);
    assert_eq!(events[0][0], FieldValue::List(vec![]));
    assert_eq!(events[0][1], FieldValue::Map(vec![]));
}
