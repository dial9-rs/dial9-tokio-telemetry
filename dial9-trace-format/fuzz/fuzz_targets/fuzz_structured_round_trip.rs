#![no_main]
//! Structured round-trip fuzzer: generates random schemas, interleaves frame types
//! (schemas, events, pool strings, symbol tables) in arbitrary order, and verifies
//! every value round-trips through encode→decode.

use arbitrary::{Arbitrary, Unstructured};
use libfuzzer_sys::fuzz_target;

use dial9_trace_format::codec::SymbolEntry;
use dial9_trace_format::decoder::{DecodedFrame, Decoder};
use dial9_trace_format::encoder::{Encoder, Schema};
use dial9_trace_format::schema::FieldDef;
use dial9_trace_format::types::{FieldType, FieldValue, InternedString};

/// Varint boundary values that stress LEB128 encoding edges.
const VARINT_INTERESTING: [u64; 8] = [
    0,
    127,
    128,
    16383,
    16384,
    u32::MAX as u64,
    u64::MAX / 2,
    u64::MAX,
];

#[derive(Arbitrary, Debug, Clone, Copy)]
enum FuzzFieldType {
    I64,
    F64,
    Bool,
    String,
    Bytes,
    PooledString,
    StackFrames,
    Varint,
    StringMap,
}

impl FuzzFieldType {
    fn to_field_type(self) -> FieldType {
        match self {
            Self::I64 => FieldType::I64,
            Self::F64 => FieldType::F64,
            Self::Bool => FieldType::Bool,
            Self::String => FieldType::String,
            Self::Bytes => FieldType::Bytes,
            Self::PooledString => FieldType::PooledString,
            Self::StackFrames => FieldType::StackFrames,
            Self::Varint => FieldType::Varint,
            Self::StringMap => FieldType::StringMap,
        }
    }
}

fn gen_value(ft: FuzzFieldType, u: &mut Unstructured) -> arbitrary::Result<FieldValue> {
    Ok(match ft {
        FuzzFieldType::I64 => FieldValue::I64(u.arbitrary()?),
        FuzzFieldType::F64 => FieldValue::F64(u.arbitrary()?),
        FuzzFieldType::Bool => FieldValue::Bool(u.arbitrary()?),
        FuzzFieldType::String => {
            let len: usize = u.int_in_range(0..=32)?;
            FieldValue::String(String::from_utf8_lossy(u.bytes(len)?).into_owned())
        }
        FuzzFieldType::Bytes => {
            let len: usize = u.int_in_range(0..=32)?;
            FieldValue::Bytes(u.bytes(len)?.to_vec())
        }
        FuzzFieldType::PooledString => FieldValue::PooledString(InternedString::from_raw(u.int_in_range(0..=50)?)),
        FuzzFieldType::StackFrames => {
            let count: usize = u.int_in_range(0..=8)?;
            let mut addrs = Vec::with_capacity(count);
            for _ in 0..count {
                addrs.push(u.arbitrary()?);
            }
            FieldValue::StackFrames(addrs)
        }
        FuzzFieldType::Varint => {
            if u.ratio(1, 4)? {
                FieldValue::Varint(VARINT_INTERESTING[u.int_in_range(0..=7)?])
            } else {
                FieldValue::Varint(u.arbitrary()?)
            }
        }
        FuzzFieldType::StringMap => {
            let count: usize = u.int_in_range(0..=4)?;
            let mut pairs = Vec::with_capacity(count);
            for _ in 0..count {
                let klen: usize = u.int_in_range(0..=8)?;
                let vlen: usize = u.int_in_range(0..=8)?;
                pairs.push((u.bytes(klen)?.to_vec(), u.bytes(vlen)?.to_vec()));
            }
            FieldValue::StringMap(pairs)
        }
    })
}

/// Top-level fuzz input — Arbitrary derive handles efficient byte consumption.
#[derive(Arbitrary, Debug)]
struct FuzzInput {
    schemas: Vec<FuzzSchema>,
    actions: Vec<FuzzAction>,
}

#[derive(Arbitrary, Debug)]
struct FuzzSchema {
    fields: Vec<FuzzFieldType>,
}

#[derive(Arbitrary, Debug)]
enum FuzzAction {
    Event { schema_idx: u8 },
    PoolString(String8),
    SymbolTable(FuzzSymbol),
}

#[derive(Arbitrary, Debug)]
struct String8 {
    data: [u8; 8],
    len: u8,
}

#[derive(Arbitrary, Debug)]
struct FuzzSymbol {
    base_addr: u64,
    size: u32,
    symbol_id: u32,
}

fuzz_target!(|data: &[u8]| {
    let mut u = Unstructured::new(data);
    let input: FuzzInput = match u.arbitrary() {
        Ok(v) => v,
        Err(_) => return,
    };

    // Clamp schemas: 1–4, 0–8 fields each (allow empty schemas now)
    let fuzz_schemas: Vec<&FuzzSchema> = input.schemas.iter().take(4).collect();
    if fuzz_schemas.is_empty() {
        return;
    }
    for s in &fuzz_schemas {
        if s.fields.len() > 8 {
            return;
        }
    }

    let actions: Vec<&FuzzAction> = input.actions.iter().take(32).collect();
    if actions.is_empty() {
        return;
    }

    // --- Encode ---
    let mut enc = Encoder::new();

    let names = ["S0", "S1", "S2", "S3"];
    let mut schemas: Vec<Schema> = Vec::new();

    // Register all schemas upfront
    for (i, fuzz_schema) in fuzz_schemas.iter().enumerate() {
        let fields: Vec<FieldDef> = fuzz_schema
            .fields
            .iter()
            .enumerate()
            .map(|(j, ft)| FieldDef {
                name: format!("f{j}"),
                field_type: ft.to_field_type(),
            })
            .collect();
        schemas.push(enc.register_schema(names[i], fields).unwrap());
    }

    // Execute actions in fuzz-determined interleaved order
    let mut expected_events: Vec<(usize, Option<u64>, Vec<FieldValue>)> = Vec::new();
    for action in &actions {
        match action {
            FuzzAction::Event { schema_idx } => {
                let idx = (*schema_idx as usize) % fuzz_schemas.len();
                let fuzz_schema = &fuzz_schemas[idx];
                let values: Vec<FieldValue> = match fuzz_schema
                    .fields
                    .iter()
                    .map(|ft| gen_value(*ft, &mut u))
                    .collect::<arbitrary::Result<Vec<_>>>()
                {
                    Ok(v) => v,
                    Err(_) => return,
                };
                // Generate a timestamp and prepend it
                let ts: u64 = match u.arbitrary() {
                    Ok(v) => v,
                    Err(_) => return,
                };
                // Clamp to reasonable range to avoid overflow in delta math
                let ts = ts % (1u64 << 48);
                let mut all_values = vec![FieldValue::Varint(ts)];
                all_values.extend(values.clone());
                enc.write_event(&schemas[idx], &all_values).unwrap();
                expected_events.push((idx, Some(ts), values));
            }
            FuzzAction::PoolString(ps) => {
                let len = (ps.len % 8) as usize;
                let s = String::from_utf8_lossy(&ps.data[..len]);
                enc.intern_string(&s).unwrap();
            }
            FuzzAction::SymbolTable(sym) => {
                enc.write_symbol_table(&[SymbolEntry {
                    base_addr: sym.base_addr,
                    size: sym.size,
                    symbol_id: InternedString::from_raw(sym.symbol_id),
                }]).unwrap();
            }
        }
    }

    let bytes = enc.finish();

    // --- Decode and verify ---
    let mut dec = Decoder::new(&bytes).expect("valid header");
    let frames = dec.decode_all();

    let decoded_events: Vec<_> = frames
        .iter()
        .filter_map(|f| match f {
            DecodedFrame::Event { type_id, timestamp_ns, values, .. } => {
                Some((*type_id, *timestamp_ns, values.clone()))
            }
            _ => None,
        })
        .collect();

    assert_eq!(
        decoded_events.len(),
        expected_events.len(),
        "event count mismatch"
    );

    for (i, ((schema_idx, expected_ts, expected_vals), (_type_id, decoded_ts, decoded_vals))) in
        expected_events.iter().zip(decoded_events.iter()).enumerate()
    {
        assert_eq!(
            *expected_ts, *decoded_ts,
            "timestamp mismatch in event {i} (schema {schema_idx})"
        );
        assert_eq!(
            expected_vals.len(),
            decoded_vals.len(),
            "field count mismatch in event {i} (schema {schema_idx})"
        );
        for (j, (expected, decoded)) in expected_vals.iter().zip(decoded_vals.iter()).enumerate() {
            match (expected, decoded) {
                (FieldValue::F64(a), FieldValue::F64(b)) => {
                    assert_eq!(a.to_bits(), b.to_bits(), "f64 mismatch event {i} field {j}");
                }
                _ => {
                    assert_eq!(
                        expected, decoded,
                        "mismatch event {i} field {j} (schema {schema_idx})"
                    );
                }
            }
        }
    }
});
