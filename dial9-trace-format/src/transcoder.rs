//! Transcoding: decode one encoded stream and re-encode through a target encoder.
//!
//! Handles string pool remapping, timestamp rebasing, and schema deduplication.
//! This is a zero-copy operation — field values are not converted to owned types.

use crate::codec::{self, WireTypeId};
use crate::decoder::{DecodeError, Decoder, StringPool};
use crate::encoder::{Encoder, Schema};
use crate::types::FieldValueRef;
use std::collections::HashMap;
use std::fmt;
use std::io::{self, Write};

/// Error during transcoding.
#[derive(Debug)]
pub enum TranscodeError {
    InvalidHeader,
    Decode(DecodeError),
    Io(io::Error),
}

impl fmt::Display for TranscodeError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            TranscodeError::InvalidHeader => write!(f, "invalid trace header"),
            TranscodeError::Decode(e) => write!(f, "{e}"),
            TranscodeError::Io(e) => write!(f, "{e}"),
        }
    }
}

impl std::error::Error for TranscodeError {}

impl From<io::Error> for TranscodeError {
    fn from(e: io::Error) -> Self {
        TranscodeError::Io(e)
    }
}

impl From<DecodeError> for TranscodeError {
    fn from(e: DecodeError) -> Self {
        TranscodeError::Decode(e)
    }
}

/// Transcode all events from `source` bytes into `target` encoder.
///
/// Decodes the source stream, remaps pooled string IDs through the target
/// encoder's string pool, and re-encodes events using [`Encoder::write_event_ref`].
/// Timestamps are decoded to absolute values by the decoder and re-encoded as
/// deltas by the target encoder. Schemas are deduplicated via
/// [`Encoder::ensure_registered`].
pub fn transcode<W: Write>(source: &[u8], target: &mut Encoder<W>) -> Result<(), TranscodeError> {
    let mut decoder = Decoder::new(source).ok_or(TranscodeError::InvalidHeader)?;

    // Build schema map as we encounter schema frames, before events reference them.
    let mut schemas: HashMap<WireTypeId, Schema> = HashMap::new();
    let mut values_buf: Vec<FieldValueRef<'_>> = Vec::new();

    while decoder.pos() < source.len() {
        let remaining = &source[decoder.pos()..];
        let tag = match remaining.first() {
            Some(t) => *t,
            None => break,
        };

        match tag {
            codec::TAG_EVENT => {
                // Decode event inline using the same approach as for_each_event,
                // but we control the loop so we can access our schema map.
                let ev_pos = decoder.pos();
                let mut pos = 1;
                let type_id = match remaining.get(pos..pos + 2) {
                    Some(b) => {
                        pos += 2;
                        WireTypeId(u16::from_le_bytes(b.try_into().unwrap()))
                    }
                    None => {
                        return Err(DecodeError {
                            pos: ev_pos,
                            message: "truncated event frame".into(),
                        }
                        .into());
                    }
                };

                let schema = schemas.get(&type_id).ok_or_else(|| DecodeError {
                    pos: ev_pos,
                    message: format!("unknown type_id {type_id:?}"),
                })?;

                let schema_info = decoder.schema_info(type_id).ok_or_else(|| DecodeError {
                    pos: ev_pos,
                    message: format!("no schema info for type_id {type_id:?}"),
                })?;

                let timestamp_ns = if schema_info.has_timestamp {
                    match codec::decode_u24_le(&remaining[pos..]) {
                        Some(delta) => {
                            pos += 3;
                            Some(decoder.timestamp_base_ns() + delta as u64)
                        }
                        None => {
                            return Err(DecodeError {
                                pos: ev_pos + pos,
                                message: "truncated timestamp delta".into(),
                            }
                            .into());
                        }
                    }
                } else {
                    None
                };

                values_buf.clear();
                for ft in schema_info.field_types {
                    match FieldValueRef::decode(*ft, remaining, pos) {
                        Some((val, consumed)) => {
                            values_buf.push(val);
                            pos += consumed;
                        }
                        None => {
                            return Err(DecodeError {
                                pos: ev_pos + pos,
                                message: "truncated field value".into(),
                            }
                            .into());
                        }
                    }
                }

                decoder.advance(pos);
                if let Some(ts) = timestamp_ns {
                    decoder.set_timestamp_base(ts);
                }

                // Remap pooled strings through target encoder
                remap_pooled_strings(&mut values_buf, decoder.string_pool(), target)?;

                let ts = timestamp_ns.unwrap_or(0);
                target.write_event_ref(schema, ts, &values_buf)?;
            }
            codec::TAG_TIMESTAMP_RESET => {
                let ts = match source.get(decoder.pos() + 1..decoder.pos() + 9) {
                    Some(b) => u64::from_le_bytes(b.try_into().unwrap()),
                    None => {
                        return Err(DecodeError {
                            pos: decoder.pos(),
                            message: "truncated timestamp reset".into(),
                        }
                        .into());
                    }
                };
                decoder.set_timestamp_base(ts);
                decoder.advance(9);
            }
            _ => {
                // Schema and string pool frames — let decoder process them
                // and update its internal state.
                let before_pos = decoder.pos();
                match decoder.next_frame_ref() {
                    Ok(Some(crate::decoder::DecodedFrameRef::Schema(entry))) => {
                        // Build a Schema handle for this type_id.
                        // The decoder just registered it, so we can find the type_id
                        // by checking what was registered at the position we just consumed.
                        let type_id = find_new_schema_id(&decoder, &schemas);
                        if let Some(type_id) = type_id {
                            let schema = Schema::from_entry(entry);
                            schemas.insert(type_id, schema);
                        }
                    }
                    Ok(Some(_)) => {} // string pool, symbol table — decoder handles state
                    Ok(None) => {
                        return Err(DecodeError {
                            pos: before_pos,
                            message: format!("failed to decode frame with tag 0x{tag:02x}"),
                        }
                        .into());
                    }
                    Err(e) => return Err(e.into()),
                }
            }
        }
    }
    Ok(())
}

/// Find the WireTypeId of a schema that was just registered in the decoder
/// but isn't in our local schema map yet.
fn find_new_schema_id(
    decoder: &Decoder<'_>,
    known: &HashMap<WireTypeId, Schema>,
) -> Option<WireTypeId> {
    for (wire_id, _entry) in decoder.registry().entries() {
        if !known.contains_key(&wire_id) {
            return Some(wire_id);
        }
    }
    None
}

/// Remap PooledString field values through the target encoder's string pool.
fn remap_pooled_strings<W: Write>(
    values: &mut [FieldValueRef<'_>],
    source_pool: &StringPool,
    target: &mut Encoder<W>,
) -> Result<(), io::Error> {
    for val in values.iter_mut() {
        if let FieldValueRef::PooledString(id) = val {
            let s = source_pool.get(*id).unwrap_or("<unknown interned string>");
            let new_id = target.intern_string(s)?;
            *val = FieldValueRef::PooledString(new_id);
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::encoder::Encoder;
    use crate::schema::FieldDef;
    use crate::types::{FieldType, FieldValue};

    #[test]
    fn test_transcode_round_trip() {
        // Encode some events into a "source" encoder
        let mut source_enc = Encoder::new();
        let schema = source_enc
            .register_schema(
                "TestEvent",
                vec![
                    FieldDef {
                        name: "value".into(),
                        field_type: FieldType::Varint,
                    },
                    FieldDef {
                        name: "label".into(),
                        field_type: FieldType::PooledString,
                    },
                ],
            )
            .unwrap();

        let label1 = source_enc.intern_string("hello").unwrap();
        let label2 = source_enc.intern_string("world").unwrap();

        source_enc
            .write_event(
                &schema,
                &[
                    FieldValue::Varint(1_000_000),
                    FieldValue::Varint(42),
                    FieldValue::PooledString(label1),
                ],
            )
            .unwrap();
        source_enc
            .write_event(
                &schema,
                &[
                    FieldValue::Varint(2_000_000),
                    FieldValue::Varint(99),
                    FieldValue::PooledString(label2),
                ],
            )
            .unwrap();

        let source_bytes = source_enc.finish();

        // Transcode into a target encoder
        let mut target_enc = Encoder::new();
        transcode(&source_bytes, &mut target_enc).unwrap();
        let target_bytes = target_enc.finish();

        // Decode both and compare
        let mut source_events = Vec::new();
        let mut source_dec = Decoder::new(&source_bytes).unwrap();
        source_dec
            .for_each_event(|ev| {
                let label = ev
                    .fields
                    .iter()
                    .find_map(|f| {
                        if let FieldValueRef::PooledString(id) = f {
                            ev.string_pool.get(*id).map(|s| s.to_string())
                        } else {
                            None
                        }
                    })
                    .unwrap_or_default();
                let varint = ev
                    .fields
                    .iter()
                    .find_map(|f| {
                        if let FieldValueRef::Varint(v) = f {
                            Some(*v)
                        } else {
                            None
                        }
                    })
                    .unwrap();
                source_events.push((ev.name.to_string(), ev.timestamp_ns, varint, label));
            })
            .unwrap();

        let mut target_events = Vec::new();
        let mut target_dec = Decoder::new(&target_bytes).unwrap();
        target_dec
            .for_each_event(|ev| {
                let label = ev
                    .fields
                    .iter()
                    .find_map(|f| {
                        if let FieldValueRef::PooledString(id) = f {
                            ev.string_pool.get(*id).map(|s| s.to_string())
                        } else {
                            None
                        }
                    })
                    .unwrap_or_default();
                let varint = ev
                    .fields
                    .iter()
                    .find_map(|f| {
                        if let FieldValueRef::Varint(v) = f {
                            Some(*v)
                        } else {
                            None
                        }
                    })
                    .unwrap();
                target_events.push((ev.name.to_string(), ev.timestamp_ns, varint, label));
            })
            .unwrap();

        assert_eq!(source_events, target_events);
    }

    #[test]
    fn test_transcode_empty_batch() {
        let source_enc = Encoder::new();
        let source_bytes = source_enc.finish();

        let mut target_enc = Encoder::new();
        transcode(&source_bytes, &mut target_enc).unwrap();

        let target_bytes = target_enc.finish();
        let mut count = 0;
        let mut dec = Decoder::new(&target_bytes).unwrap();
        dec.for_each_event(|_| count += 1).unwrap();
        assert_eq!(count, 0);
    }

    #[test]
    fn test_transcode_string_pool_remapping() {
        // Two source batches with the same strings but different local pool IDs
        let schema_fields = vec![FieldDef {
            name: "label".into(),
            field_type: FieldType::PooledString,
        }];

        // Batch 1: intern "hello"
        let mut enc1 = Encoder::new();
        let schema1 = enc1.register_schema("Ev", schema_fields.clone()).unwrap();
        let id1 = enc1.intern_string("hello").unwrap();
        enc1.write_event(
            &schema1,
            &[FieldValue::Varint(1_000_000), FieldValue::PooledString(id1)],
        )
        .unwrap();
        let bytes1 = enc1.finish();

        // Batch 2: intern "world" first (pool id 0), then "hello" (pool id 1)
        // — different local IDs than batch 1
        let mut enc2 = Encoder::new();
        let schema2 = enc2.register_schema("Ev", schema_fields.clone()).unwrap();
        let id_world = enc2.intern_string("world").unwrap();
        let id_hello = enc2.intern_string("hello").unwrap();
        enc2.write_event(
            &schema2,
            &[
                FieldValue::Varint(2_000_000),
                FieldValue::PooledString(id_world),
            ],
        )
        .unwrap();
        enc2.write_event(
            &schema2,
            &[
                FieldValue::Varint(3_000_000),
                FieldValue::PooledString(id_hello),
            ],
        )
        .unwrap();
        let bytes2 = enc2.finish();

        // Transcode both into one target
        let mut target = Encoder::new();
        transcode(&bytes1, &mut target).unwrap();
        transcode(&bytes2, &mut target).unwrap();
        let target_bytes = target.finish();

        // Verify: events resolve to correct strings despite different source pool IDs
        let mut labels = Vec::new();
        let mut dec = Decoder::new(&target_bytes).unwrap();
        dec.for_each_event(|ev| {
            if let Some(FieldValueRef::PooledString(id)) = ev.fields.first() {
                labels.push(ev.string_pool.get(*id).unwrap().to_string());
            }
        })
        .unwrap();
        assert_eq!(labels, vec!["hello", "world", "hello"]);

        // Verify string pool has exactly 2 unique strings (hello, world)
        assert_eq!(dec.string_pool().len(), 2);
    }

    #[test]
    fn test_transcode_timestamp_rebasing() {
        let fields = vec![FieldDef {
            name: "v".into(),
            field_type: FieldType::Varint,
        }];

        // Batch 1: timestamps starting at 1_000_000
        let mut enc1 = Encoder::new();
        let s1 = enc1.register_schema("Ev", fields.clone()).unwrap();
        enc1.write_event(&s1, &[FieldValue::Varint(1_000_000), FieldValue::Varint(1)])
            .unwrap();
        enc1.write_event(&s1, &[FieldValue::Varint(1_500_000), FieldValue::Varint(2)])
            .unwrap();
        let bytes1 = enc1.finish();

        // Batch 2: timestamps starting at 5_000_000
        let mut enc2 = Encoder::new();
        let s2 = enc2.register_schema("Ev", fields.clone()).unwrap();
        enc2.write_event(&s2, &[FieldValue::Varint(5_000_000), FieldValue::Varint(3)])
            .unwrap();
        let bytes2 = enc2.finish();

        let mut target = Encoder::new();
        transcode(&bytes1, &mut target).unwrap();
        transcode(&bytes2, &mut target).unwrap();
        let target_bytes = target.finish();

        let mut timestamps = Vec::new();
        let mut dec = Decoder::new(&target_bytes).unwrap();
        dec.for_each_event(|ev| {
            timestamps.push(ev.timestamp_ns.unwrap());
        })
        .unwrap();
        assert_eq!(timestamps, vec![1_000_000, 1_500_000, 5_000_000]);
    }
}
