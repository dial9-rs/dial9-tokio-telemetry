//! Integration test: encode a trace in Rust, decode it with the JS reader, compare results.

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
                FieldDef::new("a_u64", FieldType::Varint),
                FieldDef::new("b_i64", FieldType::I64),
                FieldDef::new("c_f64", FieldType::F64),
                FieldDef::new("d_bool", FieldType::Bool),
                FieldDef::new("e_string", FieldType::String),
                FieldDef::new("f_bytes", FieldType::Bytes),
                FieldDef::new("h_pooled", FieldType::PooledString),
                FieldDef::new("i_stack", FieldType::StackFrames),
                FieldDef::new("j_varint", FieldType::Varint),
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
            FieldValue::StackFrames(vec![0x1000, 0x0F00, 0x0E00].into()),
            FieldValue::Varint(999),
        ],
    )
    .unwrap();

    let mut data = enc.finish();

    // Append symbol table via a second encoder (simulating offline symbolization).
    // Decode the first trace, then create an appending encoder.
    {
        let mut decoder = dial9_trace_format::decoder::Decoder::new(&data).unwrap();
        while decoder.next_frame_ref().ok().flatten().is_some() {}
        let mut output = Vec::new();
        let mut ext = decoder.into_encoder(&mut output);
        let sym_schema = ext
            .register_schema(
                "SymbolTableEntry",
                vec![
                    FieldDef::new("base_addr", FieldType::Varint),
                    FieldDef::new("size", FieldType::Varint),
                    FieldDef::new("symbol_name", FieldType::PooledString),
                ],
            )
            .unwrap();
        ext.write_event(
            &sym_schema,
            &[
                FieldValue::Varint(0), // timestamp
                FieldValue::Varint(0x1000),
                FieldValue::Varint(256),
                FieldValue::PooledString(pool_id),
            ],
        )
        .unwrap();
        drop(ext);
        data.extend_from_slice(&output);
    }

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
        .register_schema("Ping", vec![FieldDef::new("seq", FieldType::Varint)])
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
        .register_schema("Tick", vec![FieldDef::new("ts", FieldType::Varint)])
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

#[test]
fn js_decodes_optional_pooled_string() {
    let mut enc = Encoder::new();
    let tid = enc
        .register_schema(
            "OptionalStringEvent",
            vec![
                FieldDef::new("required_id", FieldType::Varint),
                FieldDef::new("opt_name", FieldType::OptionalPooledString),
            ],
        )
        .unwrap();

    // Event with Some(interned string)
    let name_id = enc.intern_string("hello").unwrap();
    enc.write_event(
        &tid,
        &[
            FieldValue::Varint(1_000_000),
            FieldValue::Varint(42),
            FieldValue::PooledString(name_id),
        ],
    )
    .unwrap();

    // Event with None
    enc.write_event(
        &tid,
        &[
            FieldValue::Varint(2_000_000),
            FieldValue::Varint(99),
            FieldValue::None,
        ],
    )
    .unwrap();

    let data = enc.finish();
    let json = js_decode(&data);
    let frames = json["frames"].as_array().unwrap();
    let events: Vec<_> = frames.iter().filter(|f| f["type"] == "event").collect();
    assert_eq!(events.len(), 2);

    // First event: opt_name should be the resolved string, not a raw integer
    assert_eq!(events[0]["values"]["opt_name"], "hello");
    assert_eq!(events[0]["values"]["required_id"].as_str().unwrap(), "42");

    // Second event: opt_name should be null
    assert!(events[1]["values"]["opt_name"].is_null());
    assert_eq!(events[1]["values"]["required_id"].as_str().unwrap(), "99");
}

#[test]
fn js_decodes_pooled_stack_frames() {
    let mut enc = Encoder::new();
    let tid = enc
        .register_schema(
            "CpuSample",
            vec![
                FieldDef::new("worker", FieldType::Varint),
                FieldDef::new("callchain", FieldType::PooledStackFrames),
            ],
        )
        .unwrap();

    // Two distinct stacks, then reuse stack_a → exercises dedup.
    let stack_a = enc.intern_stack_frames(&[0x1000, 0x0F00, 0x0E00]).unwrap();
    let stack_b = enc.intern_stack_frames(&[0x2000, 0x2100]).unwrap();

    enc.write_event(
        &tid,
        &[
            FieldValue::Varint(1_000_000),
            FieldValue::Varint(1),
            FieldValue::PooledStackFrames(stack_a),
        ],
    )
    .unwrap();
    enc.write_event(
        &tid,
        &[
            FieldValue::Varint(2_000_000),
            FieldValue::Varint(2),
            FieldValue::PooledStackFrames(stack_b),
        ],
    )
    .unwrap();
    enc.write_event(
        &tid,
        &[
            FieldValue::Varint(3_000_000),
            FieldValue::Varint(3),
            FieldValue::PooledStackFrames(stack_a),
        ],
    )
    .unwrap();

    let data = enc.finish();
    let json = js_decode(&data);
    let frames = json["frames"].as_array().unwrap();
    let events: Vec<_> = frames.iter().filter(|f| f["type"] == "event").collect();
    assert_eq!(events.len(), 3);

    // Resolved value matches the inline `StackFrames` shape: array of decimal
    // address strings. This is what downstream JS consumers expect.
    assert_eq!(
        events[0]["values"]["callchain"],
        serde_json::json!(["4096", "3840", "3584"])
    );
    assert_eq!(
        events[1]["values"]["callchain"],
        serde_json::json!(["8192", "8448"])
    );
    // Reused pool ID resolves to same array as event 0.
    assert_eq!(
        events[2]["values"]["callchain"],
        serde_json::json!(["4096", "3840", "3584"])
    );

    // Stack pool is exposed in the CLI JSON output.
    let stack_pool = json["stackPool"].as_object().unwrap();
    assert_eq!(stack_pool.len(), 2);
    assert_eq!(
        stack_pool[&stack_a.raw_id().to_string()],
        serde_json::json!(["4096", "3840", "3584"])
    );
    assert_eq!(
        stack_pool[&stack_b.raw_id().to_string()],
        serde_json::json!(["8192", "8448"])
    );
}

#[test]
fn js_decodes_optional_pooled_stack_frames() {
    let mut enc = Encoder::new();
    let tid = enc
        .register_schema(
            "MaybeStack",
            vec![FieldDef::new(
                "callchain",
                FieldType::OptionalPooledStackFrames,
            )],
        )
        .unwrap();

    let stack = enc.intern_stack_frames(&[0xAAAA, 0xBBBB]).unwrap();
    enc.write_event(
        &tid,
        &[
            FieldValue::Varint(1_000_000),
            FieldValue::PooledStackFrames(stack),
        ],
    )
    .unwrap();
    enc.write_event(&tid, &[FieldValue::Varint(2_000_000), FieldValue::None])
        .unwrap();

    let data = enc.finish();
    let json = js_decode(&data);
    let events: Vec<_> = json["frames"]
        .as_array()
        .unwrap()
        .iter()
        .filter(|f| f["type"] == "event")
        .collect();
    assert_eq!(events.len(), 2);

    assert_eq!(
        events[0]["values"]["callchain"],
        serde_json::json!(["43690", "48059"])
    );
    assert!(events[1]["values"]["callchain"].is_null());
}

#[test]
fn js_decodes_dynamic_list_and_map() {
    let mut enc = Encoder::new();
    let tid = enc
        .register_schema(
            "Containers",
            vec![
                FieldDef::new("items", FieldType::DynamicList),
                FieldDef::new("props", FieldType::DynamicMap),
            ],
        )
        .unwrap();

    let list = FieldValue::List(vec![
        FieldValue::String("hello".into()),
        FieldValue::Varint(42),
    ]);
    let map = FieldValue::Map(vec![(
        FieldValue::String("key".into()),
        FieldValue::Varint(99),
    )]);
    enc.write_event(&tid, &[FieldValue::Varint(1000), list, map])
        .unwrap();

    let data = enc.finish();
    let json = js_decode(&data);

    let frames = json["frames"].as_array().unwrap();
    // schema + event = 2
    assert_eq!(frames.len(), 2);

    let vals = &frames[1]["values"];
    let items = vals["items"].as_array().unwrap();
    assert_eq!(items[0], "hello");
    assert_eq!(items[1], "42");

    let props = vals["props"].as_array().unwrap();
    assert_eq!(props[0][0], "key");
    assert_eq!(props[0][1], "99");
}
