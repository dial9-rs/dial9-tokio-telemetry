#![no_main]
//! Fuzz target for transcode: encode random events into one or more batches,
//! transcode them into a single target encoder, then verify decoded events match.

use arbitrary::{Arbitrary, Unstructured};
use libfuzzer_sys::fuzz_target;

use dial9_trace_format::decoder::Decoder;
use dial9_trace_format::encoder::Encoder;
use dial9_trace_format::schema::FieldDef;
use dial9_trace_format::transcoder::transcode;
use dial9_trace_format::types::{FieldType, FieldValue, FieldValueRef, InternedString};

#[derive(Arbitrary, Debug, Clone, Copy)]
enum FuzzFieldType {
    I64,
    F64,
    Bool,
    String,
    Bytes,
    PooledString,
    Varint,
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
            Self::Varint => FieldType::Varint,
        }
    }
}

#[derive(Arbitrary, Debug)]
struct FuzzInput {
    schemas: Vec<Vec<FuzzFieldType>>,
    batch_splits: Vec<u8>,
    num_events: u8,
    pool_strings: Vec<String8>,
}

#[derive(Arbitrary, Debug)]
struct String8 {
    data: [u8; 8],
    len: u8,
}

impl String8 {
    fn as_str(&self) -> String {
        let len = (self.len % 8) as usize;
        String::from_utf8_lossy(&self.data[..len]).into_owned()
    }
}

fuzz_target!(|data: &[u8]| {
    let mut u = Unstructured::new(data);
    let input: FuzzInput = match u.arbitrary() {
        Ok(v) => v,
        Err(_) => return,
    };

    let schemas: Vec<&Vec<FuzzFieldType>> = input.schemas.iter().take(4).collect();
    if schemas.is_empty() {
        return;
    }
    for s in &schemas {
        if s.len() > 6 {
            return;
        }
    }

    let num_events = (input.num_events % 24) as usize;
    if num_events == 0 {
        return;
    }

    let has_pooled = schemas
        .iter()
        .any(|s| s.iter().any(|f| matches!(f, FuzzFieldType::PooledString)));

    // Build pool strings. Intern into a scratch encoder to get the actual
    // deduplicated count (the encoder deduplicates internally).
    let raw_pool: Vec<String> = input
        .pool_strings
        .iter()
        .take(8)
        .map(|s| s.as_str())
        .collect();
    let mut scratch = Encoder::new();
    let mut pool_ids: Vec<InternedString> = Vec::new();
    for s in &raw_pool {
        pool_ids.push(scratch.intern_string_infallible(s));
    }
    // Unique IDs the encoder actually assigned
    let mut unique_ids: Vec<InternedString> = pool_ids.clone();
    unique_ids.sort_by_key(|id| id.raw_id());
    unique_ids.dedup_by_key(|id| id.raw_id());
    let max_pool_id = unique_ids.len() as u32;

    if has_pooled && max_pool_id == 0 {
        return; // no valid pool strings for PooledString fields
    }

    let schema_names = ["S0", "S1", "S2", "S3"];
    let field_defs: Vec<Vec<FieldDef>> = schemas
        .iter()
        .map(|s| {
            s.iter()
                .enumerate()
                .map(|(j, ft)| FieldDef {
                    name: format!("f{j}"),
                    field_type: ft.to_field_type(),
                })
                .collect()
        })
        .collect();

    struct EventData {
        schema_idx: usize,
        ts: u64,
        values: Vec<FieldValue>,
    }

    let mut all_events = Vec::new();
    for _ in 0..num_events {
        let schema_idx: u8 = match u.arbitrary() {
            Ok(v) => v,
            Err(_) => return,
        };
        let idx = schema_idx as usize % schemas.len();
        let ts: u64 = match u.arbitrary::<u64>() {
            Ok(v) => v % (1u64 << 48),
            Err(_) => return,
        };

        let values: Vec<FieldValue> = match schemas[idx]
            .iter()
            .map(|ft| match ft {
                FuzzFieldType::I64 => u.arbitrary().map(FieldValue::I64),
                FuzzFieldType::F64 => u.arbitrary().map(FieldValue::F64),
                FuzzFieldType::Bool => u.arbitrary().map(FieldValue::Bool),
                FuzzFieldType::String => {
                    let len: usize = u.int_in_range(0..=16)?;
                    Ok(FieldValue::String(
                        String::from_utf8_lossy(u.bytes(len)?).into_owned(),
                    ))
                }
                FuzzFieldType::Bytes => {
                    let len: usize = u.int_in_range(0..=16)?;
                    Ok(FieldValue::Bytes(u.bytes(len)?.to_vec()))
                }
                FuzzFieldType::PooledString => {
                    // Only use IDs that the encoder actually assigned
                    let pick: usize = u.int_in_range(0..=(max_pool_id as usize - 1))?;
                    Ok(FieldValue::PooledString(unique_ids[pick]))
                }
                FuzzFieldType::Varint => u.arbitrary().map(FieldValue::Varint),
            })
            .collect::<arbitrary::Result<Vec<_>>>()
        {
            Ok(v) => v,
            Err(_) => return,
        };

        all_events.push(EventData {
            schema_idx: idx,
            ts,
            values,
        });
    }

    // Determine batch boundaries
    let mut split_points: Vec<usize> = input
        .batch_splits
        .iter()
        .take(3)
        .map(|s| (*s as usize) % (num_events + 1))
        .collect();
    split_points.sort();
    split_points.dedup();
    split_points.push(num_events);

    // Encode into batches
    let mut batch_bytes: Vec<Vec<u8>> = Vec::new();
    let mut prev = 0;
    for &boundary in &split_points {
        let mut enc = Encoder::new();
        let registered: Vec<_> = field_defs
            .iter()
            .enumerate()
            .map(|(i, fields)| enc.register_schema(schema_names[i], fields.clone()).unwrap())
            .collect();

        for s in &raw_pool {
            enc.intern_string(s).unwrap();
        }

        for ev in &all_events[prev..boundary] {
            let mut all_values = vec![FieldValue::Varint(ev.ts)];
            all_values.extend(ev.values.clone());
            enc.write_event(&registered[ev.schema_idx], &all_values)
                .unwrap();
        }
        batch_bytes.push(enc.finish());
        prev = boundary;
    }

    // Transcode all batches into one target
    let mut target_enc = Encoder::new();
    for batch in &batch_bytes {
        if transcode(batch, &mut target_enc).is_err() {
            return;
        }
    }
    let target_bytes = target_enc.finish();

    // Compare: resolve PooledString through string pool for value comparison
    fn collect_events(batches: &[Vec<u8>]) -> Vec<(String, Option<u64>, Vec<String>)> {
        let mut events = Vec::new();
        for batch in batches {
            let mut dec = Decoder::new(batch).unwrap();
            dec.for_each_event(|ev| {
                let fields: Vec<String> = ev
                    .fields
                    .iter()
                    .map(|f| match f {
                        FieldValueRef::PooledString(id) => ev
                            .string_pool
                            .get(*id)
                            .unwrap_or("<missing>")
                            .to_string(),
                        other => format!("{other:?}"),
                    })
                    .collect();
                events.push((ev.name.to_string(), ev.timestamp_ns, fields));
            })
            .unwrap();
        }
        events
    }

    let source_events = collect_events(&batch_bytes);
    let target_events = collect_events(&[target_bytes]);

    assert_eq!(
        source_events.len(),
        target_events.len(),
        "event count mismatch"
    );

    for (i, (src, tgt)) in source_events.iter().zip(target_events.iter()).enumerate() {
        assert_eq!(src.0, tgt.0, "name mismatch at event {i}");
        assert_eq!(src.1, tgt.1, "timestamp mismatch at event {i}");
        assert_eq!(src.2, tgt.2, "fields mismatch at event {i}");
    }
});
