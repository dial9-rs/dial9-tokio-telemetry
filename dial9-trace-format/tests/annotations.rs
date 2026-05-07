use dial9_trace_format::codec::WireTypeId;
use dial9_trace_format::decoder::{DecodedFrame, DecodedFrameRef, Decoder};
use dial9_trace_format::encoder::{Encoder, Schema};
use dial9_trace_format::schema::{FieldAnnotation, FieldDef, SchemaEntry};
use dial9_trace_format::types::{FieldType, FieldValue};

#[test]
fn annotations_round_trip() {
    let mut enc = Encoder::new();
    let entry = SchemaEntry::with_annotations(
        "Latency",
        true,
        vec![
            FieldDef::new("duration_us", FieldType::Varint),
            FieldDef::new("endpoint", FieldType::PooledString),
        ],
        vec![
            FieldAnnotation::new(0, "metrique.unit", "microseconds"),
            FieldAnnotation::new(1, "dial9.display", "label"),
            FieldAnnotation::new(0, "dial9.kpi", "true"),
        ],
    );
    let schema = Schema::from_entry(entry);
    enc.register_existing(&schema).unwrap();

    let endpoint_id = enc.intern_string("/api/health").unwrap();
    enc.write_event(
        &schema,
        &[
            FieldValue::Varint(1_000_000),
            FieldValue::Varint(42),
            FieldValue::PooledString(endpoint_id),
        ],
    )
    .unwrap();

    let data = enc.finish();
    let mut dec = Decoder::new(&data).unwrap();
    let frames = dec.decode_all();

    // Should see: Schema, SchemaAnnotations, StringPool, Event
    let mut saw_schema = false;
    let mut saw_annotations = false;
    for frame in &frames {
        match frame {
            DecodedFrame::Schema(s) => {
                assert_eq!(s.name(), "Latency");
                saw_schema = true;
            }
            DecodedFrame::SchemaAnnotations {
                type_id,
                annotations,
            } => {
                assert_eq!(*type_id, WireTypeId(0));
                assert_eq!(annotations.len(), 3);
                assert_eq!(annotations[0].field_index(), 0);
                assert_eq!(annotations[0].key(), "metrique.unit");
                assert_eq!(annotations[0].value(), "microseconds");
                assert_eq!(annotations[1].field_index(), 1);
                assert_eq!(annotations[1].key(), "dial9.display");
                assert_eq!(annotations[1].value(), "label");
                assert_eq!(annotations[2].field_index(), 0);
                assert_eq!(annotations[2].key(), "dial9.kpi");
                assert_eq!(annotations[2].value(), "true");
                saw_annotations = true;
            }
            _ => {}
        }
    }
    assert!(saw_schema);
    assert!(saw_annotations);

    // Verify annotations are merged into the registry
    let registry_entry = dec.registry().get(WireTypeId(0)).unwrap();
    assert_eq!(registry_entry.annotations().len(), 3);
    assert_eq!(registry_entry.annotations()[0].key(), "metrique.unit");
}

#[test]
fn no_annotations_no_frame() {
    let mut enc = Encoder::new();
    enc.register_schema("Simple", vec![FieldDef::new("x", FieldType::Varint)])
        .unwrap();

    let data = enc.finish();
    let mut dec = Decoder::new(&data).unwrap();
    let frames = dec.decode_all();

    // With no annotations, no annotation frame should be emitted
    assert!(
        !frames
            .iter()
            .any(|f| matches!(f, DecodedFrame::SchemaAnnotations { .. })),
        "annotation frame should not appear when annotations are empty"
    );
}

#[test]
fn multiple_schemas_with_mixed_annotations() {
    let mut enc = Encoder::new();

    let annotated = SchemaEntry::with_annotations(
        "Annotated",
        true,
        vec![FieldDef::new("val", FieldType::Varint)],
        vec![FieldAnnotation::new(0, "unit", "ms")],
    );
    let plain = SchemaEntry::with_annotations(
        "Plain",
        true,
        vec![FieldDef::new("val", FieldType::Varint)],
        Vec::new(),
    );

    let schema_a = Schema::from_entry(annotated);
    let schema_b = Schema::from_entry(plain);
    enc.register_existing(&schema_a).unwrap();
    enc.register_existing(&schema_b).unwrap();

    let data = enc.finish();
    let mut dec = Decoder::new(&data).unwrap();
    let frames = dec.decode_all();

    // Only one annotation frame (for "Annotated")
    let annotation_frames: Vec<_> = frames
        .iter()
        .filter(|f| matches!(f, DecodedFrame::SchemaAnnotations { .. }))
        .collect();
    assert_eq!(annotation_frames.len(), 1);

    // Annotations attached to the right schema
    let annotated_entry = dec.registry().get(WireTypeId(0)).unwrap();
    assert_eq!(annotated_entry.name(), "Annotated");
    assert_eq!(annotated_entry.annotations().len(), 1);

    let plain_entry = dec.registry().get(WireTypeId(1)).unwrap();
    assert_eq!(plain_entry.name(), "Plain");
    assert!(plain_entry.annotations().is_empty());
}

#[test]
fn annotations_silent_truncation() {
    // Encode a trace with annotations, then truncate before the annotation frame.
    let mut enc = Encoder::new();
    let entry = SchemaEntry::with_annotations(
        "Ev",
        true,
        vec![FieldDef::new("x", FieldType::Varint)],
        vec![FieldAnnotation::new(0, "key", "value")],
    );
    let schema = Schema::from_entry(entry);
    enc.register_existing(&schema).unwrap();
    let data = enc.finish();

    // Find the annotation frame tag and truncate just before it
    let ann_pos = data
        .iter()
        .position(|&b| b == 0x06)
        .expect("annotation frame should exist");
    let truncated = &data[..ann_pos];

    // Decoder should halt cleanly at end-of-stream (no error, just fewer frames)
    let mut dec = Decoder::new(truncated).unwrap();
    let frames = dec.decode_all();
    // Should have the schema but not the annotations
    assert!(frames.iter().any(|f| matches!(f, DecodedFrame::Schema(_))));
    assert!(
        !frames
            .iter()
            .any(|f| matches!(f, DecodedFrame::SchemaAnnotations { .. }))
    );
}

#[test]
fn annotations_unknown_type_id_skipped() {
    // Build a valid trace, then manually append an annotation frame for an unknown type_id.
    let mut enc = Encoder::new();
    let schema = enc
        .register_schema("Real", vec![FieldDef::new("x", FieldType::Varint)])
        .unwrap();

    enc.write_event(
        &schema,
        &[FieldValue::Varint(1_000_000), FieldValue::Varint(42)],
    )
    .unwrap();

    let mut data = enc.finish();

    // Manually append an annotation frame for type_id 99 (never registered)
    // Format: tag(1) | type_id(varint) | count(2 LE) | entries...
    // Entry: field_index(2 LE) | key_len(2 LE) | key | value_len(4 LE) | value
    let key = b"ghost";
    let value = b"annotation";
    data.push(0x06); // TAG_SCHEMA_ANNOTATIONS
    data.push(99); // type_id = 99 (varint, fits in 1 byte)
    data.extend_from_slice(&1u16.to_le_bytes()); // count = 1
    data.extend_from_slice(&0u16.to_le_bytes()); // field_index = 0
    data.extend_from_slice(&(key.len() as u16).to_le_bytes());
    data.extend_from_slice(key);
    data.extend_from_slice(&(value.len() as u32).to_le_bytes());
    data.extend_from_slice(value);

    // Decoder should handle the unknown type_id leniently
    let mut dec = Decoder::new(&data).unwrap();
    let frames = dec.decode_all();

    // Should have Schema, Event, and SchemaAnnotations (for unknown type_id)
    let has_event = frames
        .iter()
        .any(|f| matches!(f, DecodedFrame::Event { .. }));
    assert!(has_event, "decoder should continue past unknown annotation");

    // The "Real" schema should have no annotations
    let real_entry = dec.registry().get(WireTypeId(0)).unwrap();
    assert!(real_entry.annotations().is_empty());
}

#[test]
fn annotations_round_trip_ref() {
    // Verify the zero-copy path also works
    let mut enc = Encoder::new();
    let entry = SchemaEntry::with_annotations(
        "Ev",
        true,
        vec![FieldDef::new("x", FieldType::Varint)],
        vec![FieldAnnotation::new(0, "key", "val")],
    );
    let schema = Schema::from_entry(entry);
    enc.register_existing(&schema).unwrap();
    enc.write_event(
        &schema,
        &[FieldValue::Varint(1_000_000), FieldValue::Varint(7)],
    )
    .unwrap();

    let data = enc.finish();
    let mut dec = Decoder::new(&data).unwrap();
    let frames = dec.decode_all_ref();

    let ann_frame = frames
        .iter()
        .find(|f| matches!(f, DecodedFrameRef::SchemaAnnotations { .. }));
    assert!(ann_frame.is_some());
    if let Some(DecodedFrameRef::SchemaAnnotations { annotations, .. }) = ann_frame {
        assert_eq!(annotations.len(), 1);
        assert_eq!(annotations[0].key(), "key");
        assert_eq!(annotations[0].value(), "val");
    }
}

#[test]
fn annotations_for_each_event_works() {
    // Verify that for_each_event processes events correctly when annotations are present
    let mut enc = Encoder::new();
    let entry = SchemaEntry::with_annotations(
        "Metric",
        true,
        vec![FieldDef::new("val", FieldType::Varint)],
        vec![FieldAnnotation::new(0, "unit", "bytes")],
    );
    let schema = Schema::from_entry(entry);
    enc.register_existing(&schema).unwrap();
    enc.write_event(
        &schema,
        &[FieldValue::Varint(1_000_000), FieldValue::Varint(1024)],
    )
    .unwrap();

    let data = enc.finish();
    let mut dec = Decoder::new(&data).unwrap();
    let mut event_count = 0;
    dec.for_each_event(|ev| {
        assert_eq!(ev.name, "Metric");
        // Annotations should be visible on the schema
        assert_eq!(ev.schema.annotations().len(), 1);
        assert_eq!(ev.schema.annotations()[0].key(), "unit");
        event_count += 1;
    })
    .unwrap();
    assert_eq!(event_count, 1);
}
