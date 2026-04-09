//! Wire-format encoding and decoding for trace frames.
//!
//! This module contains the low-level frame codec. Most users should use
//! [`Encoder`](crate::encoder::Encoder) and [`Decoder`](crate::decoder::Decoder)
//! instead. The types [`WireTypeId`], [`PoolEntry`], and [`PoolEntryRef`] are
//! re-exported here because they appear in the decoder's public API.

use crate::schema::{FieldDef, SchemaEntry};
use crate::types::{FieldType, FieldValue, FieldValueRef};
use std::io::{self, Write};

/// Type ID as it appears on the wire (u16 in schema/event frame headers).
/// Assigned sequentially by the encoder; the decoder reads them from the stream.
///
/// ## Note
/// The wire type id is only stable within a single file. It is not static.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct WireTypeId(pub u16);

pub(crate) const MAGIC: [u8; 4] = [0x54, 0x52, 0x43, 0x00]; // TRC\0
pub(crate) const VERSION: u8 = 1;
pub(crate) const HEADER_SIZE: usize = 5;

pub(crate) const TAG_SCHEMA: u8 = 0x01;
pub(crate) const TAG_EVENT: u8 = 0x02;
pub(crate) const TAG_STRING_POOL: u8 = 0x03;
// Tags 0x04 and 0x06 are reserved (formerly SymbolTable and ProcMaps, now schema-based events).
pub(crate) const TAG_TIMESTAMP_RESET: u8 = 0x05;

/// Maximum nanosecond delta that fits in a u24 (3 bytes).
pub(crate) const MAX_TIMESTAMP_DELTA_NS: u64 = 0xFF_FFFF; // 16,777,215

/// Encode a u32 value as 3-byte little-endian (u24). Caller must ensure `value <= 0xFF_FFFF`.
#[inline]
pub(crate) fn encode_u24_le(value: u32, w: &mut impl Write) -> io::Result<()> {
    debug_assert!(value <= MAX_TIMESTAMP_DELTA_NS as u32);
    w.write_all(&[value as u8, (value >> 8) as u8, (value >> 16) as u8])
}

/// Decode a 3-byte little-endian u24 from `data`. Returns `None` if fewer than 3 bytes.
#[inline]
pub(crate) fn decode_u24_le(data: &[u8]) -> Option<u32> {
    let b = data.get(..3)?;
    Some(b[0] as u32 | (b[1] as u32) << 8 | (b[2] as u32) << 16)
}

/// An owned string pool entry.
#[derive(Debug, Clone, PartialEq)]
pub struct PoolEntry {
    /// Pool ID assigned by the encoder.
    pub pool_id: u32,
    /// Raw string data.
    pub data: Vec<u8>,
}

#[derive(Debug, Clone, PartialEq)]
pub(crate) enum Frame {
    Schema {
        type_id: WireTypeId,
        entry: SchemaEntry,
    },
    Event {
        type_id: WireTypeId,
        /// Absolute timestamp in nanoseconds, if the schema has `has_timestamp`.
        timestamp_ns: Option<u64>,
        values: Vec<FieldValue>,
    },
    StringPool(Vec<PoolEntry>),
    TimestampReset(u64),
}

/// Zero-copy pool entry borrowing from the input buffer.
#[non_exhaustive]
#[derive(Debug, Clone, PartialEq)]
pub struct PoolEntryRef<'a> {
    /// Pool ID assigned by the encoder.
    pub pool_id: u32,
    /// Raw string data borrowed from the decode buffer.
    pub data: &'a [u8],
}

/// Zero-copy frame that borrows from the input buffer.
#[derive(Debug, Clone, PartialEq)]
pub(crate) enum FrameRef<'a> {
    Schema {
        type_id: WireTypeId,
        entry: SchemaEntry,
    },
    Event {
        type_id: WireTypeId,
        timestamp_ns: Option<u64>,
        values: Vec<FieldValueRef<'a>>,
    },
    StringPool(Vec<PoolEntryRef<'a>>),
    TimestampReset(u64),
}

/// Schema info needed by the decoder: field types + has_timestamp flag.
pub(crate) struct SchemaInfo<'a> {
    pub field_types: &'a [FieldType],
    pub has_timestamp: bool,
}

// --- Encoding ---

pub(crate) fn encode_header(w: &mut impl Write) -> io::Result<()> {
    w.write_all(&MAGIC)?;
    w.write_all(&[VERSION])
}

pub(crate) fn encode_schema(
    type_id: WireTypeId,
    entry: &SchemaEntry,
    w: &mut impl Write,
) -> io::Result<()> {
    w.write_all(&[TAG_SCHEMA])?;
    w.write_all(&type_id.0.to_le_bytes())?;
    let name_bytes = entry.name.as_bytes();
    w.write_all(&(name_bytes.len() as u16).to_le_bytes())?;
    w.write_all(name_bytes)?;
    w.write_all(&[if entry.has_timestamp { 1 } else { 0 }])?;
    w.write_all(&(entry.fields.len() as u16).to_le_bytes())?;
    for f in &entry.fields {
        let fname = f.name.as_bytes();
        w.write_all(&(fname.len() as u16).to_le_bytes())?;
        w.write_all(fname)?;
        w.write_all(&[f.field_type as u8])?;
    }
    Ok(())
}

/// Encode an event frame. If `timestamp_delta_ns` is Some, writes a u24 LE delta
/// after the type_id (for schemas with `has_timestamp = true`).
#[cfg(test)]
pub(crate) fn encode_event(
    type_id: WireTypeId,
    timestamp_delta_ns: Option<u32>,
    values: &[FieldValue],
    w: &mut impl Write,
) -> io::Result<()> {
    w.write_all(&[TAG_EVENT])?;
    w.write_all(&type_id.0.to_le_bytes())?;
    if let Some(delta) = timestamp_delta_ns {
        encode_u24_le(delta, w)?;
    }
    for v in values {
        v.encode(w)?;
    }
    Ok(())
}

pub(crate) fn encode_string_pool(entries: &[PoolEntry], w: &mut impl Write) -> io::Result<()> {
    w.write_all(&[TAG_STRING_POOL])?;
    w.write_all(&(entries.len() as u32).to_le_bytes())?;
    for e in entries {
        w.write_all(&e.pool_id.to_le_bytes())?;
        w.write_all(&(e.data.len() as u32).to_le_bytes())?;
        w.write_all(&e.data)?;
    }
    Ok(())
}

// --- Decoding ---

pub(crate) fn decode_header(data: &[u8]) -> Option<u8> {
    if data.get(..4)? != MAGIC {
        return None;
    }
    let version = *data.get(4)?;
    Some(version)
}

/// Decode a single frame starting at `data`. Returns (Frame, bytes_consumed).
pub(crate) fn decode_frame<'s>(
    data: &[u8],
    schema_lookup: impl Fn(WireTypeId) -> Option<SchemaInfo<'s>>,
    timestamp_base_ns: u64,
) -> Option<(Frame, usize)> {
    let tag = *data.first()?;
    match tag {
        TAG_SCHEMA => decode_schema_frame(data),
        TAG_EVENT => decode_event_frame(data, schema_lookup, timestamp_base_ns),
        TAG_STRING_POOL => decode_string_pool_frame(data),
        TAG_TIMESTAMP_RESET => {
            let ts = u64::from_le_bytes(data.get(1..9)?.try_into().ok()?);
            Some((Frame::TimestampReset(ts), 9))
        }
        _ => None,
    }
}

fn decode_schema_frame(data: &[u8]) -> Option<(Frame, usize)> {
    let mut pos = 1; // skip tag
    let type_id = WireTypeId(u16::from_le_bytes(data.get(pos..pos + 2)?.try_into().ok()?));
    pos += 2;
    let name_len = u16::from_le_bytes(data.get(pos..pos + 2)?.try_into().ok()?) as usize;
    pos += 2;
    let name = String::from_utf8(data.get(pos..pos + name_len)?.to_vec()).ok()?;
    pos += name_len;
    let has_timestamp = *data.get(pos)? != 0;
    pos += 1;
    let field_count = u16::from_le_bytes(data.get(pos..pos + 2)?.try_into().ok()?) as usize;
    pos += 2;
    let mut fields = Vec::with_capacity(field_count);
    for _ in 0..field_count {
        let fname_len = u16::from_le_bytes(data.get(pos..pos + 2)?.try_into().ok()?) as usize;
        pos += 2;
        let fname = String::from_utf8(data.get(pos..pos + fname_len)?.to_vec()).ok()?;
        pos += fname_len;
        let ft = FieldType::from_tag(*data.get(pos)?)?;
        pos += 1;
        fields.push(FieldDef {
            name: fname,
            field_type: ft,
        });
    }
    Some((
        Frame::Schema {
            type_id,
            entry: SchemaEntry {
                name,
                has_timestamp,
                fields,
            },
        },
        pos,
    ))
}

fn decode_event_frame<'s>(
    data: &[u8],
    schema_lookup: impl Fn(WireTypeId) -> Option<SchemaInfo<'s>>,
    timestamp_base_ns: u64,
) -> Option<(Frame, usize)> {
    let mut pos = 1; // skip tag
    let type_id = WireTypeId(u16::from_le_bytes(data.get(pos..pos + 2)?.try_into().ok()?));
    pos += 2;
    let info = schema_lookup(type_id)?;

    let timestamp_ns = if info.has_timestamp {
        let delta = decode_u24_le(&data[pos..])?;
        pos += 3;
        Some(timestamp_base_ns.checked_add(delta as u64)?)
    } else {
        None
    };

    let mut values = Vec::with_capacity(info.field_types.len());
    let mut remaining = &data[pos..];
    for ft in info.field_types {
        let (val, rest) = FieldValue::decode(*ft, remaining)?;
        values.push(val);
        remaining = rest;
    }
    let consumed = data.len() - remaining.len();
    Some((
        Frame::Event {
            type_id,
            timestamp_ns,
            values,
        },
        consumed,
    ))
}

fn decode_string_pool_frame(data: &[u8]) -> Option<(Frame, usize)> {
    let mut pos = 1;
    let count = u32::from_le_bytes(data.get(pos..pos + 4)?.try_into().ok()?) as usize;
    pos += 4;
    let mut entries = Vec::with_capacity(count.min((data.len() - pos) / 8));
    for _ in 0..count {
        let pool_id = u32::from_le_bytes(data.get(pos..pos + 4)?.try_into().ok()?);
        pos += 4;
        let len = u32::from_le_bytes(data.get(pos..pos + 4)?.try_into().ok()?) as usize;
        pos += 4;
        let d = data.get(pos..pos + len)?.to_vec();
        pos += len;
        entries.push(PoolEntry { pool_id, data: d });
    }
    Some((Frame::StringPool(entries), pos))
}

// --- Zero-copy decoding ---

/// Decode a single frame without allocating owned data for field values.
pub(crate) fn decode_frame_ref<'a, 's>(
    data: &'a [u8],
    schema_lookup: impl Fn(WireTypeId) -> Option<SchemaInfo<'s>>,
    timestamp_base_ns: u64,
) -> Option<(FrameRef<'a>, usize)> {
    let tag = *data.first()?;
    match tag {
        TAG_SCHEMA => {
            let (frame, consumed) = decode_schema_frame(data)?;
            match frame {
                Frame::Schema { type_id, entry } => {
                    Some((FrameRef::Schema { type_id, entry }, consumed))
                }
                _ => unreachable!(),
            }
        }
        TAG_EVENT => decode_event_frame_ref(data, schema_lookup, timestamp_base_ns),
        TAG_STRING_POOL => decode_string_pool_frame_ref(data),
        TAG_TIMESTAMP_RESET => {
            let ts = u64::from_le_bytes(data.get(1..9)?.try_into().ok()?);
            Some((FrameRef::TimestampReset(ts), 9))
        }
        _ => None,
    }
}

fn decode_event_frame_ref<'a, 's>(
    data: &'a [u8],
    schema_lookup: impl Fn(WireTypeId) -> Option<SchemaInfo<'s>>,
    timestamp_base_ns: u64,
) -> Option<(FrameRef<'a>, usize)> {
    let mut pos = 1;
    let type_id = WireTypeId(u16::from_le_bytes(data.get(pos..pos + 2)?.try_into().ok()?));
    pos += 2;
    let info = schema_lookup(type_id)?;

    let timestamp_ns = if info.has_timestamp {
        let delta = decode_u24_le(&data[pos..])?;
        pos += 3;
        Some(timestamp_base_ns.checked_add(delta as u64)?)
    } else {
        None
    };

    let mut values = Vec::with_capacity(info.field_types.len());
    for ft in info.field_types {
        let (val, consumed) = FieldValueRef::decode(*ft, data, pos)?;
        values.push(val);
        pos += consumed;
    }
    Some((
        FrameRef::Event {
            type_id,
            timestamp_ns,
            values,
        },
        pos,
    ))
}

fn decode_string_pool_frame_ref<'a>(data: &'a [u8]) -> Option<(FrameRef<'a>, usize)> {
    let mut pos = 1;
    let count = u32::from_le_bytes(data.get(pos..pos + 4)?.try_into().ok()?) as usize;
    pos += 4;
    let mut entries = Vec::with_capacity(count.min((data.len() - pos) / 8));
    for _ in 0..count {
        let pool_id = u32::from_le_bytes(data.get(pos..pos + 4)?.try_into().ok()?);
        pos += 4;
        let len = u32::from_le_bytes(data.get(pos..pos + 4)?.try_into().ok()?) as usize;
        pos += 4;
        let d = data.get(pos..pos + len)?;
        pos += len;
        entries.push(PoolEntryRef { pool_id, data: d });
    }
    Some((FrameRef::StringPool(entries), pos))
}

#[cfg(test)]
mod tests {
    use super::*;

    // --- Header tests ---

    #[test]
    fn header_encode_decode() {
        let mut buf = Vec::new();
        encode_header(&mut buf).unwrap();
        assert_eq!(buf, [0x54, 0x52, 0x43, 0x00, 1]);
        assert_eq!(decode_header(&buf), Some(1));
    }

    #[test]
    fn header_bad_magic() {
        assert_eq!(decode_header(&[0x00, 0x00, 0x00, 0x00, 1]), None);
    }

    #[test]
    fn header_too_short() {
        assert_eq!(decode_header(&[0x54, 0x52]), None);
    }

    // --- Schema frame tests ---

    #[test]
    fn schema_frame_round_trip() {
        let type_id = WireTypeId(1);
        let entry = SchemaEntry {
            name: "PollStart".into(),
            has_timestamp: true,
            fields: vec![FieldDef {
                name: "worker".into(),
                field_type: FieldType::Varint,
            }],
        };
        let mut buf = Vec::new();
        encode_schema(type_id, &entry, &mut buf).unwrap();
        assert_eq!(buf[0], TAG_SCHEMA);
        let (frame, consumed) = decode_frame(&buf, |_| None, 0).unwrap();
        assert_eq!(consumed, buf.len());
        assert_eq!(frame, Frame::Schema { type_id, entry });
    }

    #[test]
    fn schema_frame_empty_fields() {
        let type_id = WireTypeId(0);
        let entry = SchemaEntry {
            name: "Empty".into(),
            has_timestamp: false,
            fields: vec![],
        };
        let mut buf = Vec::new();
        encode_schema(type_id, &entry, &mut buf).unwrap();
        let (frame, _) = decode_frame(&buf, |_| None, 0).unwrap();
        assert_eq!(frame, Frame::Schema { type_id, entry });
    }

    // --- Event frame tests ---

    #[test]
    fn event_frame_round_trip() {
        let values = vec![
            FieldValue::Varint(12345),
            FieldValue::Bool(true),
            FieldValue::String("hi".to_string()),
        ];
        let mut buf = Vec::new();
        encode_event(WireTypeId(1), None, &values, &mut buf).unwrap();
        assert_eq!(buf[0], TAG_EVENT);

        let types = vec![FieldType::Varint, FieldType::Bool, FieldType::String];
        let lookup = |id: WireTypeId| -> Option<SchemaInfo<'_>> {
            if id == WireTypeId(1) {
                Some(SchemaInfo {
                    field_types: &types,
                    has_timestamp: false,
                })
            } else {
                None
            }
        };
        let (frame, consumed) = decode_frame(&buf, lookup, 0).unwrap();
        assert_eq!(consumed, buf.len());
        assert_eq!(
            frame,
            Frame::Event {
                type_id: WireTypeId(1),
                timestamp_ns: None,
                values
            }
        );
    }

    #[test]
    fn event_frame_with_timestamp_round_trip() {
        let values = vec![FieldValue::Varint(42)];
        let mut buf = Vec::new();
        encode_event(WireTypeId(1), Some(1_000_000), &values, &mut buf).unwrap();

        let types = vec![FieldType::Varint];
        let lookup = |id: WireTypeId| -> Option<SchemaInfo<'_>> {
            if id == WireTypeId(1) {
                Some(SchemaInfo {
                    field_types: &types,
                    has_timestamp: true,
                })
            } else {
                None
            }
        };
        let (frame, consumed) = decode_frame(&buf, lookup, 5_000_000).unwrap();
        assert_eq!(consumed, buf.len());
        assert_eq!(
            frame,
            Frame::Event {
                type_id: WireTypeId(1),
                timestamp_ns: Some(5_000_000 + 1_000_000),
                values,
            }
        );
    }

    #[test]
    fn event_frame_unknown_type_id() {
        let mut buf = Vec::new();
        encode_event(WireTypeId(99), None, &[FieldValue::Varint(1)], &mut buf).unwrap();
        assert!(decode_frame(&buf, |_| None, 0).is_none());
    }

    #[test]
    fn event_frame_varint_compact() {
        let values = vec![FieldValue::Varint(1_050_000), FieldValue::Varint(3)];
        let mut buf = Vec::new();
        encode_event(WireTypeId(2), None, &values, &mut buf).unwrap();
        assert!(
            buf.len() <= 7,
            "varint PollEnd should be <=7 bytes, got {}",
            buf.len()
        );

        let types = vec![FieldType::Varint, FieldType::Varint];
        let lookup = |id: WireTypeId| -> Option<SchemaInfo<'_>> {
            if id == WireTypeId(2) {
                Some(SchemaInfo {
                    field_types: &types,
                    has_timestamp: false,
                })
            } else {
                None
            }
        };
        let (frame, consumed) = decode_frame(&buf, lookup, 0).unwrap();
        assert_eq!(consumed, buf.len());
        assert_eq!(
            frame,
            Frame::Event {
                type_id: WireTypeId(2),
                timestamp_ns: None,
                values
            }
        );
    }

    // --- String pool frame tests ---

    #[test]
    fn string_pool_round_trip() {
        let entries = vec![
            PoolEntry {
                pool_id: 0,
                data: b"main_thread".to_vec(),
            },
            PoolEntry {
                pool_id: 1,
                data: b"worker-1".to_vec(),
            },
        ];
        let mut buf = Vec::new();
        encode_string_pool(&entries, &mut buf).unwrap();
        assert_eq!(buf[0], TAG_STRING_POOL);
        let (frame, consumed) = decode_frame(&buf, |_| None, 0).unwrap();
        assert_eq!(consumed, buf.len());
        assert_eq!(frame, Frame::StringPool(entries));
    }

    #[test]
    fn string_pool_empty() {
        let mut buf = Vec::new();
        encode_string_pool(&[], &mut buf).unwrap();
        let (frame, _) = decode_frame(&buf, |_| None, 0).unwrap();
        assert_eq!(frame, Frame::StringPool(vec![]));
    }

    #[test]
    fn unknown_tag_returns_none() {
        assert!(decode_frame(&[0xFF], |_| None, 0).is_none());
    }

    #[test]
    fn truncated_event_frame() {
        let types = vec![FieldType::Varint];
        let data = [TAG_EVENT, 0x01];
        let result = decode_frame(
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
        let data = [TAG_SCHEMA, 0x00, 0x00];
        let result = decode_frame(&data, |_: WireTypeId| None, 0);
        assert!(result.is_none());
    }
}
