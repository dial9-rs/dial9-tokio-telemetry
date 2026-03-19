//! Integration test: encode a trace in Rust, decode it with the JS reader, compare results.

use dial9_trace_format::codec::{self, WireTypeId};
use dial9_trace_format::encoder::Encoder;
use dial9_trace_format::schema::FieldDef;
use dial9_trace_format::types::{FieldType, FieldValue};
use std::io::Write;
use std::process::Command;

fn js_decode(trace: &[u8]) -> serde_json::Value {
    let mut tmp = tempfile::NamedTempFile::new().unwrap();
    tmp.write_all(trace).unwrap();
    let js_path = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("js/decode.js");
    let output = Command::new("node")
        .arg(&js_path)
        .arg(tmp.path())
        .output()
        .expect("failed to run node");
    assert!(
        output.status.success(),
        "node failed: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    serde_json::from_slice(&output.stdout).expect("invalid JSON from JS decoder")
}

#[test]
fn js_decodes_all_field_types() {
    let mut enc = Encoder::new();
    let tid = enc
        .register_schema(
            "AllTypes",
            vec![
                FieldDef {
                    name: "a_u64".into(),
                    field_type: FieldType::Varint,
                },
                FieldDef {
                    name: "b_i64".into(),
                    field_type: FieldType::I64,
                },
                FieldDef {
                    name: "c_f64".into(),
                    field_type: FieldType::F64,
                },
                FieldDef {
                    name: "d_bool".into(),
                    field_type: FieldType::Bool,
                },
                FieldDef {
                    name: "e_string".into(),
                    field_type: FieldType::String,
                },
                FieldDef {
                    name: "f_bytes".into(),
                    field_type: FieldType::Bytes,
                },
                FieldDef {
                    name: "h_pooled".into(),
                    field_type: FieldType::PooledString,
                },
                FieldDef {
                    name: "i_stack".into(),
                    field_type: FieldType::StackFrames,
                },
                FieldDef {
                    name: "j_varint".into(),
                    field_type: FieldType::Varint,
                },
            ],
        )
        .unwrap();

    let pool_id = enc.intern_string("hello").unwrap();
    enc.write_event(
        &tid,
        &[
            FieldValue::Varint(1_000_000), // timestamp
            FieldValue::Varint(42),
            FieldValue::I64(-7),
            FieldValue::F64(std::f64::consts::PI),
            FieldValue::Bool(true),
            FieldValue::String("world".to_string()),
            FieldValue::Bytes(vec![0xDE, 0xAD]),
            FieldValue::PooledString(pool_id),
            FieldValue::StackFrames(vec![0x1000, 0x0F00, 0x0E00]),
            FieldValue::Varint(999),
        ],
    )
    .unwrap();

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
            FieldValue::Varint(0x1000),
            FieldValue::Varint(256),
            FieldValue::PooledString(pool_id),
        ],
        &mut data,
    )
    .unwrap();

    let json = js_decode(&data);

    assert_eq!(json["version"], 1);

    let frames = json["frames"].as_array().unwrap();
    // schema(AllTypes) + string_pool + event + schema(SymbolTableEntry) + symbol_event = 5
    assert_eq!(frames.len(), 5);

    assert_eq!(frames[0]["type"], "schema");
    assert_eq!(frames[0]["name"], "AllTypes");
    assert_eq!(frames[0]["fields"].as_array().unwrap().len(), 9);

    let vals = &frames[2]["values"];
    assert_eq!(vals["a_u64"], "42");
    assert_eq!(vals["b_i64"], "-7");
    assert!((vals["c_f64"].as_f64().unwrap() - std::f64::consts::PI).abs() < 1e-10);
    assert_eq!(vals["d_bool"], true);
    assert_eq!(vals["e_string"], "world");
    assert_eq!(vals["f_bytes"], serde_json::json!([0xDE, 0xAD]));
    assert_eq!(vals["h_pooled"], "hello");
    assert_eq!(vals["i_stack"], serde_json::json!(["4096", "3840", "3584"]));
    assert_eq!(vals["j_varint"], "999");

    let sym = &frames[4]["values"];
    assert_eq!(sym["base_addr"], "4096");
    assert_eq!(sym["size"], "256");
    assert_eq!(sym["symbol_name"], "hello");
}

#[test]
fn js_decodes_empty_stream() {
    let data = Encoder::new().finish();
    let json = js_decode(&data);
    assert_eq!(json["version"], 1);
    assert_eq!(json["frames"].as_array().unwrap().len(), 0);
}

#[test]
fn js_decodes_truncated_trace_gracefully() {
    let mut enc = Encoder::new();
    let tid = enc
        .register_schema(
            "Ping",
            vec![FieldDef {
                name: "seq".into(),
                field_type: FieldType::Varint,
            }],
        )
        .unwrap();
    // Write two events so the first one is fully decodable.
    for i in 0..2u64 {
        enc.write_event(
            &tid,
            &[FieldValue::Varint(i * 1_000_000), FieldValue::Varint(i)],
        )
        .unwrap();
    }
    let full = enc.finish();

    // Chop off the last few bytes to simulate a truncated final frame.
    let truncated = &full[..full.len() - 3];
    let json = js_decode(truncated);

    assert_eq!(json["version"], 1);
    let frames = json["frames"].as_array().unwrap();
    // Schema + first event should survive; the truncated second event is dropped.
    assert!(
        frames.len() >= 2,
        "expected at least schema + one event, got {}",
        frames.len()
    );
    let events: Vec<_> = frames.iter().filter(|f| f["type"] == "event").collect();
    assert!(
        !events.is_empty(),
        "expected at least one successfully decoded event"
    );
}

#[test]
fn js_decodes_multiple_events() {
    let mut enc = Encoder::new();
    let tid = enc
        .register_schema(
            "Tick",
            vec![FieldDef {
                name: "ts".into(),
                field_type: FieldType::Varint,
            }],
        )
        .unwrap();
    for i in 0..5u64 {
        enc.write_event(
            &tid,
            &[
                FieldValue::Varint(i * 1_000_000),
                FieldValue::Varint(i * 1000),
            ],
        )
        .unwrap();
    }
    let data = enc.finish();
    let json = js_decode(&data);
    let frames = json["frames"].as_array().unwrap();
    assert_eq!(frames.len(), 6);
    let events: Vec<_> = frames.iter().filter(|f| f["type"] == "event").collect();
    assert_eq!(events.len(), 5);
    assert_eq!(events[0]["values"]["ts"], "0");
    assert_eq!(events[4]["values"]["ts"], "4000");
}
