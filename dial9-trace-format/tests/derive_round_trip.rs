//! Kitchen-sink round-trip tests: encode via derive macro, decode via
//! TraceEvent::decode() into typed Ref structs, verify every field type,
//! edge cases, multi-event streams, and interleaved string pools.

use dial9_trace_format::decoder::{DecodedFrame, DecodedFrameRef, Decoder};
use dial9_trace_format::encoder::Encoder;
use dial9_trace_format::types::{FieldType, FieldValueRef};
use dial9_trace_format::{InternedString, StackFrames, TraceEvent};

// ── Structs covering every supported field type ─────────────────────────

#[derive(TraceEvent)]
struct KitchenSink {
    #[traceevent(timestamp)]
    timestamp_ns: u64,
    a_u8: u8,
    b_u16: u16,
    c_u32: u32,
    d_u64: u64,
    e_i64: i64,
    f_f64: f64,
    g_bool: bool,
    h_string: String,
    i_bytes: Vec<u8>,
    j_interned: InternedString,
    k_frames: StackFrames,
    l_map: Vec<(String, String)>,
}

#[derive(TraceEvent)]
struct Empty {
    #[traceevent(timestamp)]
    timestamp_ns: u64,
}

#[derive(TraceEvent)]
struct SingleVarint {
    #[traceevent(timestamp)]
    timestamp_ns: u64,
    v: u64,
}

#[derive(TraceEvent)]
struct TwoStrings {
    #[traceevent(timestamp)]
    timestamp_ns: u64,
    a: String,
    b: String,
}

#[derive(TraceEvent)]
struct MultiPool {
    #[traceevent(timestamp)]
    timestamp_ns: u64,
    x: InternedString,
    y: InternedString,
    z: InternedString,
}

// ── Helpers ─────────────────────────────────────────────────────────────

/// Decode all ref frames, returning only events as (type_id, values) pairs.
fn decode_events(data: &[u8]) -> (Decoder<'_>, Vec<(Option<u64>, Vec<FieldValueRef<'_>>)>) {
    let mut dec = Decoder::new(data).unwrap();
    let mut events = Vec::new();
    for frame in dec.decode_all_ref() {
        if let DecodedFrameRef::Event {
            timestamp_ns,
            values,
            ..
        } = frame
        {
            events.push((timestamp_ns, values));
        }
    }
    (dec, events)
}

// ── Tests ───────────────────────────────────────────────────────────────

#[test]
fn kitchen_sink_all_field_types() {
    let mut enc = Encoder::new();
    let interned = enc.intern_string("hello_pool").unwrap();

    let ev = KitchenSink {
        timestamp_ns: 1_000_000,
        a_u8: 255,
        b_u16: 65535,
        c_u32: 0xDEAD_BEEF,
        d_u64: u64::MAX,
        e_i64: i64::MIN,
        f_f64: std::f64::consts::PI,
        g_bool: true,
        h_string: "hello world 🌍".to_string(),
        i_bytes: vec![0x00, 0xFF, 0xAB, 0xCD],
        j_interned: interned,
        k_frames: StackFrames(vec![0x5555_5555_1234, 0x5555_5555_0a00, 0x5555_5555_0800]),
        l_map: vec![
            ("key1".to_string(), "val1".to_string()),
            ("key2".to_string(), "val2".to_string()),
        ],
    };
    enc.write(&ev).unwrap();
    let data = enc.finish();

    let mut dec = Decoder::new(&data).unwrap();
    let frames = dec.decode_all_ref();
    let (ts, event) = frames
        .iter()
        .find_map(|f| match f {
            DecodedFrameRef::Event {
                timestamp_ns,
                values,
                ..
            } => Some((*timestamp_ns, values)),
            _ => None,
        })
        .unwrap();

    let decoded = KitchenSink::decode(ts, event).unwrap();

    assert_eq!(decoded.a_u8, 255);
    assert_eq!(decoded.b_u16, 65535);
    assert_eq!(decoded.c_u32, 0xDEAD_BEEF);
    assert_eq!(decoded.d_u64, u64::MAX);
    assert_eq!(decoded.e_i64, i64::MIN);
    assert!((decoded.f_f64 - std::f64::consts::PI).abs() < f64::EPSILON);
    assert!(decoded.g_bool);
    assert_eq!(decoded.h_string, "hello world 🌍");
    assert_eq!(decoded.i_bytes, &[0x00, 0xFF, 0xAB, 0xCD]);
    assert_eq!(decoded.j_interned, interned);
    assert_eq!(
        decoded.k_frames.iter().collect::<Vec<_>>(),
        vec![0x5555_5555_1234, 0x5555_5555_0a00, 0x5555_5555_0800]
    );
    let map_pairs: Vec<_> = decoded.l_map.iter().collect();
    assert_eq!(map_pairs, vec![("key1", "val1"), ("key2", "val2"),]);

    // Verify pool was resolved
    assert_eq!(dec.string_pool().get(interned), Some("hello_pool"));
}

#[test]
fn kitchen_sink_zero_and_empty_values() {
    let mut enc = Encoder::new();
    let interned = enc.intern_string("").unwrap();

    let ev = KitchenSink {
        timestamp_ns: 1_000,
        a_u8: 0,
        b_u16: 0,
        c_u32: 0,
        d_u64: 0,
        e_i64: 0,
        f_f64: 0.0,
        g_bool: false,
        h_string: String::new(),
        i_bytes: vec![],
        j_interned: interned,
        k_frames: StackFrames(vec![]),
        l_map: vec![],
    };
    enc.write(&ev).unwrap();
    let data = enc.finish();

    let mut dec = Decoder::new(&data).unwrap();
    let frames = dec.decode_all_ref();
    let (ts, event) = frames
        .iter()
        .find_map(|f| match f {
            DecodedFrameRef::Event {
                timestamp_ns,
                values,
                ..
            } => Some((*timestamp_ns, values)),
            _ => None,
        })
        .unwrap();

    let decoded = KitchenSink::decode(ts, event).unwrap();

    assert_eq!(decoded.a_u8, 0);
    assert_eq!(decoded.b_u16, 0);
    assert_eq!(decoded.c_u32, 0);
    assert_eq!(decoded.d_u64, 0);
    assert_eq!(decoded.e_i64, 0);
    assert_eq!(decoded.f_f64, 0.0);
    assert!(!decoded.g_bool);
    assert_eq!(decoded.h_string, "");
    assert!(decoded.i_bytes.is_empty());
    assert_eq!(decoded.k_frames.count(), 0);
    assert_eq!(decoded.l_map.count(), 0);
}

#[test]
fn empty_struct_round_trip() {
    let mut enc = Encoder::new();
    enc.write(&Empty {
        timestamp_ns: 1_000,
    })
    .unwrap();
    enc.write(&Empty {
        timestamp_ns: 2_000,
    })
    .unwrap();
    let data = enc.finish();

    let (_, events) = decode_events(&data);
    assert_eq!(events.len(), 2);
    for (ts, values) in &events {
        let decoded = Empty::decode(*ts, values).unwrap();
        let _ = decoded; // just verify it decodes
    }
}

#[test]
fn multi_event_stream_preserves_order() {
    let mut enc = Encoder::new();
    for i in 0..100u64 {
        enc.write(&SingleVarint {
            timestamp_ns: i * 1_000,
            v: i,
        })
        .unwrap();
    }
    let data = enc.finish();

    let (_, events) = decode_events(&data);
    assert_eq!(events.len(), 100);
    for (i, (ts, values)) in events.iter().enumerate() {
        let decoded = SingleVarint::decode(*ts, values).unwrap();
        assert_eq!(decoded.v, i as u64);
    }
}

#[test]
fn interleaved_event_types() {
    let mut enc = Encoder::new();
    // Alternate between two event types
    for i in 0..20u64 {
        enc.write(&SingleVarint {
            timestamp_ns: i * 2_000,
            v: i,
        })
        .unwrap();
        enc.write(&TwoStrings {
            timestamp_ns: i * 2_000 + 1_000,
            a: format!("a{i}"),
            b: format!("b{i}"),
        })
        .unwrap();
    }
    let data = enc.finish();

    let mut dec = Decoder::new(&data).unwrap();
    let frames = dec.decode_all_ref();
    let events: Vec<_> = frames
        .iter()
        .filter_map(|f| match f {
            DecodedFrameRef::Event {
                type_id,
                timestamp_ns,
                values,
                ..
            } => Some((*type_id, *timestamp_ns, values)),
            _ => None,
        })
        .collect();

    assert_eq!(events.len(), 40);
    // Events alternate: SingleVarint, TwoStrings, SingleVarint, TwoStrings...
    let varint_tid = events[0].0;
    let strings_tid = events[1].0;
    assert_ne!(varint_tid, strings_tid);

    for i in 0..20 {
        let (tid, ts, vals) = &events[i * 2];
        assert_eq!(*tid, varint_tid);
        let sv = SingleVarint::decode(*ts, vals).unwrap();
        assert_eq!(sv.v, i as u64);

        let (tid, ts, vals) = &events[i * 2 + 1];
        assert_eq!(*tid, strings_tid);
        let decoded = TwoStrings::decode(*ts, vals).unwrap();
        assert_eq!(decoded.a, format!("a{i}"));
        assert_eq!(decoded.b, format!("b{i}"));
    }
}

#[test]
fn interleaved_string_pools() {
    let mut enc = Encoder::new();
    // Intern strings interleaved with events that reference them
    let id_a = enc.intern_string("alpha").unwrap();
    enc.write(&MultiPool {
        timestamp_ns: 1_000,
        x: id_a,
        y: id_a,
        z: id_a,
    })
    .unwrap();

    let id_b = enc.intern_string("beta").unwrap();
    enc.write(&MultiPool {
        timestamp_ns: 2_000,
        x: id_a,
        y: id_b,
        z: id_a,
    })
    .unwrap();

    let id_c = enc.intern_string("gamma").unwrap();
    enc.write(&MultiPool {
        timestamp_ns: 3_000,
        x: id_c,
        y: id_b,
        z: id_a,
    })
    .unwrap();

    let data = enc.finish();

    let mut dec = Decoder::new(&data).unwrap();
    let frames = dec.decode_all_ref();
    let events: Vec<_> = frames
        .iter()
        .filter_map(|f| match f {
            DecodedFrameRef::Event {
                timestamp_ns,
                values,
                ..
            } => Some((*timestamp_ns, values)),
            _ => None,
        })
        .collect();

    assert_eq!(events.len(), 3);

    let e0 = MultiPool::decode(events[0].0, events[0].1).unwrap();
    assert_eq!(e0.x, id_a);
    assert_eq!(e0.y, id_a);
    assert_eq!(e0.z, id_a);

    let e1 = MultiPool::decode(events[1].0, events[1].1).unwrap();
    assert_eq!(e1.x, id_a);
    assert_eq!(e1.y, id_b);
    assert_eq!(e1.z, id_a);

    let e2 = MultiPool::decode(events[2].0, events[2].1).unwrap();
    assert_eq!(e2.x, id_c);
    assert_eq!(e2.y, id_b);
    assert_eq!(e2.z, id_a);

    // All pool entries resolved
    assert_eq!(dec.string_pool().get(id_a).unwrap(), "alpha");
    assert_eq!(dec.string_pool().get(id_b).unwrap(), "beta");
    assert_eq!(dec.string_pool().get(id_c).unwrap(), "gamma");
}

#[test]
fn string_pool_deduplication() {
    let mut enc = Encoder::new();
    let id1 = enc.intern_string("same").unwrap();
    let id2 = enc.intern_string("same").unwrap();
    let id3 = enc.intern_string("different").unwrap();
    assert_eq!(id1, id2);
    assert_ne!(id1, id3);

    enc.write(&MultiPool {
        timestamp_ns: 1_000,
        x: id1,
        y: id2,
        z: id3,
    })
    .unwrap();
    let data = enc.finish();

    let mut dec = Decoder::new(&data).unwrap();
    let frames = dec.decode_all_ref();
    let (ts, event) = frames
        .iter()
        .find_map(|f| match f {
            DecodedFrameRef::Event {
                timestamp_ns,
                values,
                ..
            } => Some((*timestamp_ns, values)),
            _ => None,
        })
        .unwrap();

    let decoded = MultiPool::decode(ts, event).unwrap();
    assert_eq!(decoded.x, decoded.y); // same pool ID
    assert_ne!(decoded.x, decoded.z);
}

#[test]
fn stack_frames_edge_cases() {
    #[derive(TraceEvent)]
    struct FrameEvent {
        #[traceevent(timestamp)]
        timestamp_ns: u64,
        frames: StackFrames,
    }

    let cases: Vec<Vec<u64>> = vec![
        vec![],                                                     // empty
        vec![0],                                                    // single zero
        vec![u64::MAX],                                             // max address
        vec![0x1000; 100],                                          // 100 identical
        vec![0x5555_5555_5000, 0x5555_5555_4000, 0x5555_5555_3000], // descending
        vec![1, 2, 3, 4, 5],                                        // ascending small
        vec![0, u64::MAX, 0, u64::MAX],                             // alternating extremes
    ];

    for (idx, addrs) in cases.iter().enumerate() {
        let mut enc = Encoder::new();
        enc.write(&FrameEvent {
            timestamp_ns: idx as u64 * 1_000,
            frames: StackFrames(addrs.clone()),
        })
        .unwrap();
        let data = enc.finish();

        let mut dec = Decoder::new(&data).unwrap();
        let frames = dec.decode_all_ref();
        let (ts, event) = frames
            .iter()
            .find_map(|f| match f {
                DecodedFrameRef::Event {
                    timestamp_ns,
                    values,
                    ..
                } => Some((*timestamp_ns, values)),
                _ => None,
            })
            .unwrap();

        let decoded = FrameEvent::decode(ts, event).unwrap();
        let decoded_addrs: Vec<u64> = decoded.frames.iter().collect();
        assert_eq!(&decoded_addrs, addrs, "failed for input {addrs:?}");
    }
}

#[test]
fn f64_special_values_round_trip() {
    #[derive(TraceEvent)]
    struct Floats {
        #[traceevent(timestamp)]
        timestamp_ns: u64,
        a: f64,
        b: f64,
        c: f64,
        d: f64,
    }

    let mut enc = Encoder::new();
    enc.write(&Floats {
        timestamp_ns: 1_000,
        a: f64::INFINITY,
        b: f64::NEG_INFINITY,
        c: -0.0,
        d: f64::MIN_POSITIVE,
    })
    .unwrap();
    let data = enc.finish();

    let mut dec = Decoder::new(&data).unwrap();
    let frames = dec.decode_all_ref();
    let (ts, event) = frames
        .iter()
        .find_map(|f| match f {
            DecodedFrameRef::Event {
                timestamp_ns,
                values,
                ..
            } => Some((*timestamp_ns, values)),
            _ => None,
        })
        .unwrap();

    let decoded = Floats::decode(ts, event).unwrap();
    assert_eq!(decoded.a, f64::INFINITY);
    assert_eq!(decoded.b, f64::NEG_INFINITY);
    assert!(decoded.c.is_sign_negative() && decoded.c == 0.0);
    assert_eq!(decoded.d, f64::MIN_POSITIVE);
}

#[test]
fn f64_nan_round_trip() {
    #[derive(TraceEvent)]
    struct NanEvent {
        #[traceevent(timestamp)]
        timestamp_ns: u64,
        v: f64,
    }

    let mut enc = Encoder::new();
    enc.write(&NanEvent {
        timestamp_ns: 1_000,
        v: f64::NAN,
    })
    .unwrap();
    let data = enc.finish();

    let mut dec = Decoder::new(&data).unwrap();
    let frames = dec.decode_all_ref();
    let (ts, event) = frames
        .iter()
        .find_map(|f| match f {
            DecodedFrameRef::Event {
                timestamp_ns,
                values,
                ..
            } => Some((*timestamp_ns, values)),
            _ => None,
        })
        .unwrap();

    let decoded = NanEvent::decode(ts, event).unwrap();
    assert!(decoded.v.is_nan());
}

#[test]
fn unicode_string_round_trip() {
    let mut enc = Encoder::new();
    enc.write(&TwoStrings {
        timestamp_ns: 1_000,
        a: "日本語テスト 🎌".to_string(),
        b: "émojis: 🦀🔥💯 and ñ".to_string(),
    })
    .unwrap();
    let data = enc.finish();

    let mut dec = Decoder::new(&data).unwrap();
    let frames = dec.decode_all_ref();
    let (ts, event) = frames
        .iter()
        .find_map(|f| match f {
            DecodedFrameRef::Event {
                timestamp_ns,
                values,
                ..
            } => Some((*timestamp_ns, values)),
            _ => None,
        })
        .unwrap();

    let decoded = TwoStrings::decode(ts, event).unwrap();
    assert_eq!(decoded.a, "日本語テスト 🎌");
    assert_eq!(decoded.b, "émojis: 🦀🔥💯 and ñ");
}

#[test]
fn large_bytes_field() {
    #[derive(TraceEvent)]
    struct BlobEvent {
        #[traceevent(timestamp)]
        timestamp_ns: u64,
        data: Vec<u8>,
    }

    let big = vec![0xABu8; 64 * 1024]; // 64KB
    let mut enc = Encoder::new();
    enc.write(&BlobEvent {
        timestamp_ns: 1_000,
        data: big.clone(),
    })
    .unwrap();
    let data = enc.finish();

    let mut dec = Decoder::new(&data).unwrap();
    let frames = dec.decode_all_ref();
    let (ts, event) = frames
        .iter()
        .find_map(|f| match f {
            DecodedFrameRef::Event {
                timestamp_ns,
                values,
                ..
            } => Some((*timestamp_ns, values)),
            _ => None,
        })
        .unwrap();

    let decoded = BlobEvent::decode(ts, event).unwrap();
    assert_eq!(decoded.data.len(), 64 * 1024);
    assert!(decoded.data.iter().all(|&b| b == 0xAB));
}

#[test]
fn varint_boundary_values() {
    #[derive(TraceEvent)]
    struct Boundaries {
        #[traceevent(timestamp)]
        timestamp_ns: u64,
        a: u8,
        b: u16,
        c: u32,
        d: u64,
    }

    let mut enc = Encoder::new();
    // LEB128 boundaries: 127/128, 16383/16384
    enc.write(&Boundaries {
        timestamp_ns: 1_000,
        a: 127,
        b: 128,
        c: 16383,
        d: 16384,
    })
    .unwrap();
    enc.write(&Boundaries {
        timestamp_ns: 2_000,
        a: u8::MAX,
        b: u16::MAX,
        c: u32::MAX,
        d: u64::MAX,
    })
    .unwrap();
    enc.write(&Boundaries {
        timestamp_ns: 3_000,
        a: 0,
        b: 0,
        c: 0,
        d: 0,
    })
    .unwrap();
    let data = enc.finish();

    let (_, events) = decode_events(&data);
    assert_eq!(events.len(), 3);

    let d0 = Boundaries::decode(events[0].0, &events[0].1).unwrap();
    assert_eq!((d0.a, d0.b, d0.c, d0.d), (127, 128, 16383, 16384));

    let d1 = Boundaries::decode(events[1].0, &events[1].1).unwrap();
    assert_eq!(
        (d1.a, d1.b, d1.c, d1.d),
        (u8::MAX, u16::MAX, u32::MAX, u64::MAX)
    );

    let d2 = Boundaries::decode(events[2].0, &events[2].1).unwrap();
    assert_eq!((d2.a, d2.b, d2.c, d2.d), (0, 0, 0, 0));
}

#[test]
fn decode_wrong_field_count_returns_none() {
    // Manually construct a FieldValueRef slice that's too short
    let vals: Vec<FieldValueRef<'_>> = vec![FieldValueRef::Varint(1)];
    // TwoStrings expects 2 fields, giving it 1 should fail
    assert!(TwoStrings::decode(None, &vals).is_none());
    // Empty slice
    assert!(SingleVarint::decode(None, &[]).is_none());
}

#[test]
fn string_map_round_trip() {
    #[derive(TraceEvent)]
    struct MapEvent {
        #[traceevent(timestamp)]
        timestamp_ns: u64,
        tags: Vec<(String, String)>,
    }

    let mut enc = Encoder::new();
    enc.write(&MapEvent {
        timestamp_ns: 1_000,
        tags: vec![
            ("env".into(), "prod".into()),
            ("region".into(), "us-east-1".into()),
            ("service".into(), "api-gateway".into()),
        ],
    })
    .unwrap();
    // Empty map
    enc.write(&MapEvent {
        timestamp_ns: 2_000,
        tags: vec![],
    })
    .unwrap();
    // Unicode keys/values
    enc.write(&MapEvent {
        timestamp_ns: 3_000,
        tags: vec![("名前".into(), "テスト".into())],
    })
    .unwrap();
    let data = enc.finish();

    let (_, events) = decode_events(&data);
    assert_eq!(events.len(), 3);

    let d0 = MapEvent::decode(events[0].0, &events[0].1).unwrap();
    let pairs0: Vec<_> = d0.tags.iter().collect();
    assert_eq!(
        pairs0,
        vec![
            ("env", "prod"),
            ("region", "us-east-1"),
            ("service", "api-gateway"),
        ]
    );

    let d1 = MapEvent::decode(events[1].0, &events[1].1).unwrap();
    assert_eq!(d1.tags.count(), 0);

    let d2 = MapEvent::decode(events[2].0, &events[2].1).unwrap();
    let pairs2: Vec<_> = d2.tags.iter().collect();
    assert_eq!(pairs2, vec![("名前", "テスト")]);
}

#[test]
fn many_events_many_types_stress() {
    #[derive(TraceEvent)]
    struct TypeA {
        #[traceevent(timestamp)]
        ts: u64,
        val: u32,
    }
    #[derive(TraceEvent)]
    struct TypeB {
        #[traceevent(timestamp)]
        timestamp_ns: u64,
        name: String,
        flag: bool,
    }
    #[derive(TraceEvent)]
    struct TypeC {
        #[traceevent(timestamp)]
        timestamp_ns: u64,
        x: f64,
        y: f64,
    }

    let mut enc = Encoder::new();
    let n = 500;
    for i in 0..n {
        match i % 3 {
            0 => enc
                .write(&TypeA {
                    ts: i as u64 * 1000,
                    val: i as u32,
                })
                .unwrap(),
            1 => enc
                .write(&TypeB {
                    timestamp_ns: i as u64 * 1000,
                    name: format!("ev{i}"),
                    flag: i % 2 == 0,
                })
                .unwrap(),
            2 => enc
                .write(&TypeC {
                    timestamp_ns: i as u64 * 1000,
                    x: i as f64 * 0.1,
                    y: -(i as f64),
                })
                .unwrap(),
            _ => unreachable!(),
        }
    }
    let data = enc.finish();

    let mut dec = Decoder::new(&data).unwrap();
    let frames = dec.decode_all_ref();
    let events: Vec<_> = frames
        .iter()
        .filter_map(|f| match f {
            DecodedFrameRef::Event {
                type_id,
                timestamp_ns,
                values,
                ..
            } => Some((*type_id, *timestamp_ns, values)),
            _ => None,
        })
        .collect();
    assert_eq!(events.len(), n);

    // Resolve type_ids by name
    let a_name = TypeA::event_name();
    let b_name = TypeB::event_name();
    let c_name = TypeC::event_name();

    for (idx, (tid, ts, vals)) in events.iter().enumerate() {
        let schema_name = &dec.registry().get(*tid).unwrap().name;
        match idx % 3 {
            0 => {
                assert_eq!(schema_name, a_name);
                let d = TypeA::decode(*ts, vals).unwrap();
                assert_eq!(d.ts, idx as u64 * 1000);
                assert_eq!(d.val, idx as u32);
            }
            1 => {
                assert_eq!(schema_name, b_name);
                let d = TypeB::decode(*ts, vals).unwrap();
                assert_eq!(d.name, format!("ev{idx}"));
                assert_eq!(d.flag, idx % 2 == 0);
            }
            2 => {
                assert_eq!(schema_name, c_name);
                let d = TypeC::decode(*ts, vals).unwrap();
                assert!((d.x - idx as f64 * 0.1).abs() < f64::EPSILON);
                assert!((d.y - -(idx as f64)).abs() < f64::EPSILON);
            }
            _ => unreachable!(),
        }
    }
}

#[test]
fn timestamp_backwards_via_derive_write() {
    #[derive(TraceEvent)]
    struct Ev {
        #[traceevent(timestamp)]
        timestamp_ns: u64,
        v: u64,
    }

    let mut enc = Encoder::new();
    // First event at 20ms — forces a reset (delta from base 0 > 16.7ms)
    enc.write(&Ev {
        timestamp_ns: 20_000_000,
        v: 1,
    })
    .unwrap();
    // Second event goes backwards to 19ms — base is now 20_000_000,
    // saturating_sub gives 0, so without the fix this decodes as 20_000_000
    enc.write(&Ev {
        timestamp_ns: 19_000_000,
        v: 2,
    })
    .unwrap();
    let bytes = enc.finish();

    let mut dec = Decoder::new(&bytes).unwrap();
    let events: Vec<_> = dec
        .decode_all()
        .into_iter()
        .filter_map(|f| match f {
            DecodedFrame::Event {
                timestamp_ns,
                values,
                ..
            } => Some((timestamp_ns, values)),
            _ => None,
        })
        .collect();

    assert_eq!(events.len(), 2);
    assert_eq!(events[0].0, Some(20_000_000));
    assert_eq!(
        events[1].0,
        Some(19_000_000),
        "backwards timestamp must be preserved via reset, not clamped to base"
    );
}

#[test]
fn encoder_new_to_with_preallocated_buffer() {
    let buf = Vec::with_capacity(64);
    let mut enc = Encoder::new_to(buf).unwrap();
    enc.write(&SingleVarint {
        timestamp_ns: 1_000,
        v: 42,
    })
    .unwrap();
    let data = enc.into_inner();

    let (_, events) = decode_events(&data);
    assert_eq!(events.len(), 1);
    let decoded = SingleVarint::decode(events[0].0, &events[0].1).unwrap();
    assert_eq!(decoded.v, 42);
}

#[test]
fn derive_timestamp_round_trip() {
    #[derive(TraceEvent)]
    struct Ev {
        #[traceevent(timestamp)]
        timestamp_ns: u64,
        v: u64,
    }

    let mut enc = Encoder::new();
    let timestamps = [
        0u64,
        1_000,
        5_000,
        20_000_000,
        20_001_000,
        100_000_000,
        50_000_000,
        50_001_000,
    ];
    for (i, &ts) in timestamps.iter().enumerate() {
        enc.write(&Ev {
            timestamp_ns: ts,
            v: i as u64,
        })
        .unwrap();
    }
    let bytes = enc.finish();

    let mut dec = Decoder::new(&bytes).unwrap();
    let frames = dec.decode_all_ref();
    let events: Vec<_> = frames
        .iter()
        .filter_map(|f| match f {
            DecodedFrameRef::Event {
                timestamp_ns,
                values,
                ..
            } => Some((*timestamp_ns, values)),
            _ => None,
        })
        .collect();

    assert_eq!(events.len(), timestamps.len());
    for (i, (ts, vals)) in events.iter().enumerate() {
        assert_eq!(*ts, Some(timestamps[i]), "timestamp mismatch at event {i}");
        let decoded = Ev::decode(*ts, vals).unwrap();
        assert_eq!(decoded.timestamp_ns, timestamps[i]);
        assert_eq!(decoded.v, i as u64);
    }
}

#[test]
fn for_each_event_with_derive_types() {
    #[derive(TraceEvent)]
    struct Ev {
        #[traceevent(timestamp)]
        timestamp_ns: u64,
        worker_id: u64,
        name: String,
    }

    let mut enc = Encoder::new();
    enc.write(&Ev {
        timestamp_ns: 1_000,
        worker_id: 0,
        name: "a".into(),
    })
    .unwrap();
    enc.write(&Ev {
        timestamp_ns: 2_000,
        worker_id: 1,
        name: "b".into(),
    })
    .unwrap();
    let bytes = enc.finish();

    let mut dec = Decoder::new(&bytes).unwrap();
    let mut results = Vec::new();
    dec.for_each_event(|ev| {
        let decoded = Ev::decode(ev.timestamp_ns, ev.fields).unwrap();
        results.push((
            decoded.timestamp_ns,
            decoded.worker_id,
            decoded.name.to_string(),
        ));
    })
    .unwrap();

    assert_eq!(results.len(), 2);
    assert_eq!(results[0], (1_000, 0, "a".to_string()));
    assert_eq!(results[1], (2_000, 1, "b".to_string()));
}

#[test]
fn for_each_event_resolves_interned_strings() {
    #[derive(TraceEvent)]
    struct TaskStart {
        #[traceevent(timestamp)]
        timestamp_ns: u64,
        task_id: u64,
        spawn_location: InternedString,
    }

    let mut enc = Encoder::new();
    let loc_a = enc.intern_string("src/main.rs:42").unwrap();
    let loc_b = enc.intern_string("src/lib.rs:10").unwrap();
    enc.write(&TaskStart {
        timestamp_ns: 1_000,
        task_id: 1,
        spawn_location: loc_a,
    })
    .unwrap();
    enc.write(&TaskStart {
        timestamp_ns: 2_000,
        task_id: 2,
        spawn_location: loc_b,
    })
    .unwrap();
    enc.write(&TaskStart {
        timestamp_ns: 3_000,
        task_id: 3,
        spawn_location: loc_a,
    })
    .unwrap();
    let bytes = enc.finish();

    let mut dec = Decoder::new(&bytes).unwrap();
    let mut results = Vec::new();
    dec.for_each_event(|ev| {
        assert_eq!(ev.name, "TaskStart");
        let decoded = TaskStart::decode(ev.timestamp_ns, ev.fields).unwrap();
        // Resolve InternedString → &str via the string pool on RawEvent
        let location = ev.string_pool.get(decoded.spawn_location).unwrap();
        results.push((decoded.task_id, location.to_string()));
    })
    .unwrap();

    assert_eq!(results.len(), 3);
    assert_eq!(results[0], (1, "src/main.rs:42".to_string()));
    assert_eq!(results[1], (2, "src/lib.rs:10".to_string()));
    assert_eq!(results[2], (3, "src/main.rs:42".to_string()));
}

// ── Tests moved from derive_test.rs (trait method + field type mapping) ─

#[derive(TraceEvent)]
struct SpanEvent {
    #[traceevent(timestamp)]
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
    assert_eq!(defs.len(), 3);
    assert_eq!(defs[0].name, "parent_id");
    assert_eq!(defs[0].field_type, FieldType::Varint);
    assert_eq!(defs[1].name, "name");
    assert_eq!(defs[1].field_type, FieldType::String);
    assert_eq!(defs[2].name, "duration_ns");
    assert_eq!(defs[2].field_type, FieldType::Varint);
}

#[derive(TraceEvent)]
struct SmallTypes {
    #[traceevent(timestamp)]
    timestamp_ns: u64,
    a: u8,
    b: u16,
    c: u32,
    d: u64,
}

#[test]
fn derive_small_int_types_use_fixed_width() {
    let defs = SmallTypes::field_defs();
    assert_eq!(defs[0].field_type, FieldType::U8, "u8 should be U8");
    assert_eq!(defs[1].field_type, FieldType::U16, "u16 should be U16");
    assert_eq!(defs[2].field_type, FieldType::U32, "u32 should be U32");
    assert_eq!(
        defs[3].field_type,
        FieldType::Varint,
        "u64 should be Varint"
    );
}
