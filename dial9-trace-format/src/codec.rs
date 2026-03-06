// Frame encoding/decoding (header, schema, event, string pool, symbol table)

use crate::schema::{FieldDef, SchemaEntry};
use crate::types::{FieldType, FieldValue, FieldValueRef};

pub const MAGIC: [u8; 4] = [0x54, 0x52, 0x43, 0x00]; // TRC\0
pub const VERSION: u8 = 1;
pub const HEADER_SIZE: usize = 5;

pub const TAG_SCHEMA: u8 = 0x01;
pub const TAG_EVENT: u8 = 0x02;
pub const TAG_STRING_POOL: u8 = 0x03;
pub const TAG_SYMBOL_TABLE: u8 = 0x04;

#[derive(Debug, Clone, PartialEq)]
pub struct PoolEntry {
    pub pool_id: u32,
    pub data: Vec<u8>,
}

#[derive(Debug, Clone, PartialEq)]
pub struct SymbolEntry {
    pub base_addr: u64,
    pub size: u32,
    pub symbol_id: u32,
}

#[derive(Debug, Clone, PartialEq)]
pub enum Frame {
    Schema(SchemaEntry),
    Event { type_id: u16, values: Vec<FieldValue> },
    StringPool(Vec<PoolEntry>),
    SymbolTable(Vec<SymbolEntry>),
}

/// Zero-copy pool entry borrowing from the input buffer.
#[derive(Debug, Clone, PartialEq)]
pub struct PoolEntryRef<'a> {
    pub pool_id: u32,
    pub data: &'a [u8],
}

/// Zero-copy frame that borrows from the input buffer.
#[derive(Debug, Clone, PartialEq)]
pub enum FrameRef<'a> {
    Schema(SchemaEntry),
    Event { type_id: u16, values: Vec<FieldValueRef<'a>> },
    StringPool(Vec<PoolEntryRef<'a>>),
    SymbolTable(Vec<SymbolEntry>),
}

// --- Encoding ---

pub fn encode_header(buf: &mut Vec<u8>) {
    buf.extend_from_slice(&MAGIC);
    buf.push(VERSION);
}

pub fn encode_schema(entry: &SchemaEntry, buf: &mut Vec<u8>) {
    buf.push(TAG_SCHEMA);
    buf.extend_from_slice(&entry.type_id.to_le_bytes());
    let name_bytes = entry.name.as_bytes();
    buf.extend_from_slice(&(name_bytes.len() as u16).to_le_bytes());
    buf.extend_from_slice(name_bytes);
    buf.extend_from_slice(&(entry.fields.len() as u16).to_le_bytes());
    for f in &entry.fields {
        let fname = f.name.as_bytes();
        buf.extend_from_slice(&(fname.len() as u16).to_le_bytes());
        buf.extend_from_slice(fname);
        buf.push(f.field_type as u8);
    }
}

pub fn encode_event(type_id: u16, values: &[FieldValue], buf: &mut Vec<u8>) {
    buf.push(TAG_EVENT);
    buf.extend_from_slice(&type_id.to_le_bytes());
    for v in values {
        v.encode(buf);
    }
}

pub fn encode_string_pool(entries: &[PoolEntry], buf: &mut Vec<u8>) {
    buf.push(TAG_STRING_POOL);
    buf.extend_from_slice(&(entries.len() as u32).to_le_bytes());
    for e in entries {
        buf.extend_from_slice(&e.pool_id.to_le_bytes());
        buf.extend_from_slice(&(e.data.len() as u32).to_le_bytes());
        buf.extend_from_slice(&e.data);
    }
}

pub fn encode_symbol_table(entries: &[SymbolEntry], buf: &mut Vec<u8>) {
    buf.push(TAG_SYMBOL_TABLE);
    buf.extend_from_slice(&(entries.len() as u32).to_le_bytes());
    for e in entries {
        buf.extend_from_slice(&e.base_addr.to_le_bytes());
        buf.extend_from_slice(&e.size.to_le_bytes());
        buf.extend_from_slice(&e.symbol_id.to_le_bytes());
    }
}

// --- Decoding ---

pub fn decode_header(data: &[u8]) -> Option<u8> {
    if data.get(..4)? != MAGIC {
        return None;
    }
    let version = *data.get(4)?;
    Some(version)
}

/// Decode a single frame starting at `data`. Returns (Frame, bytes_consumed).
pub fn decode_frame<'s>(data: &[u8], schema_lookup: &dyn Fn(u16) -> Option<&'s [FieldType]>) -> Option<(Frame, usize)> {
    let tag = *data.first()?;
    match tag {
        TAG_SCHEMA => decode_schema_frame(data),
        TAG_EVENT => decode_event_frame(data, schema_lookup),
        TAG_STRING_POOL => decode_string_pool_frame(data),
        TAG_SYMBOL_TABLE => decode_symbol_table_frame(data),
        _ => None,
    }
}

fn decode_schema_frame(data: &[u8]) -> Option<(Frame, usize)> {
    let mut pos = 1; // skip tag
    let type_id = u16::from_le_bytes(data.get(pos..pos + 2)?.try_into().ok()?);
    pos += 2;
    let name_len = u16::from_le_bytes(data.get(pos..pos + 2)?.try_into().ok()?) as usize;
    pos += 2;
    let name = String::from_utf8(data.get(pos..pos + name_len)?.to_vec()).ok()?;
    pos += name_len;
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
        fields.push(FieldDef { name: fname, field_type: ft });
    }
    Some((Frame::Schema(SchemaEntry { type_id, name, fields }), pos))
}

fn decode_event_frame<'s>(data: &[u8], schema_lookup: &dyn Fn(u16) -> Option<&'s [FieldType]>) -> Option<(Frame, usize)> {
    let mut pos = 1; // skip tag
    let type_id = u16::from_le_bytes(data.get(pos..pos + 2)?.try_into().ok()?);
    pos += 2;
    let field_types = schema_lookup(type_id)?;
    let mut values = Vec::with_capacity(field_types.len());
    for ft in field_types {
        let (val, consumed) = FieldValue::decode(*ft, data, pos)?;
        values.push(val);
        pos += consumed;
    }
    Some((Frame::Event { type_id, values }, pos))
}

fn decode_string_pool_frame(data: &[u8]) -> Option<(Frame, usize)> {
    let mut pos = 1;
    let count = u32::from_le_bytes(data.get(pos..pos + 4)?.try_into().ok()?) as usize;
    pos += 4;
    // Cap capacity to avoid OOM on malicious input (each entry is ≥8 bytes)
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

fn decode_symbol_table_frame(data: &[u8]) -> Option<(Frame, usize)> {
    let mut pos = 1;
    let count = u32::from_le_bytes(data.get(pos..pos + 4)?.try_into().ok()?) as usize;
    pos += 4;
    // Cap capacity to avoid OOM on malicious input (each entry is 16 bytes)
    let mut entries = Vec::with_capacity(count.min((data.len() - pos) / 16));
    for _ in 0..count {
        let base_addr = u64::from_le_bytes(data.get(pos..pos + 8)?.try_into().ok()?);
        pos += 8;
        let size = u32::from_le_bytes(data.get(pos..pos + 4)?.try_into().ok()?);
        pos += 4;
        let symbol_id = u32::from_le_bytes(data.get(pos..pos + 4)?.try_into().ok()?);
        pos += 4;
        entries.push(SymbolEntry { base_addr, size, symbol_id });
    }
    Some((Frame::SymbolTable(entries), pos))
}

// --- Zero-copy decoding ---

/// Decode a single frame without allocating owned data for field values.
pub fn decode_frame_ref<'a, 's>(data: &'a [u8], schema_lookup: &dyn Fn(u16) -> Option<&'s [FieldType]>) -> Option<(FrameRef<'a>, usize)> {
    let tag = *data.first()?;
    match tag {
        TAG_SCHEMA => {
            let (frame, consumed) = decode_schema_frame(data)?;
            match frame {
                Frame::Schema(s) => Some((FrameRef::Schema(s), consumed)),
                _ => None,
            }
        }
        TAG_EVENT => decode_event_frame_ref(data, schema_lookup),
        TAG_STRING_POOL => decode_string_pool_frame_ref(data),
        TAG_SYMBOL_TABLE => {
            let (frame, consumed) = decode_symbol_table_frame(data)?;
            match frame {
                Frame::SymbolTable(e) => Some((FrameRef::SymbolTable(e), consumed)),
                _ => None,
            }
        }
        _ => None,
    }
}

fn decode_event_frame_ref<'a, 's>(data: &'a [u8], schema_lookup: &dyn Fn(u16) -> Option<&'s [FieldType]>) -> Option<(FrameRef<'a>, usize)> {
    let mut pos = 1;
    let type_id = u16::from_le_bytes(data.get(pos..pos + 2)?.try_into().ok()?);
    pos += 2;
    let field_types = schema_lookup(type_id)?;
    let mut values = Vec::with_capacity(field_types.len());
    for ft in field_types {
        let (val, consumed) = FieldValueRef::decode(*ft, data, pos)?;
        values.push(val);
        pos += consumed;
    }
    Some((FrameRef::Event { type_id, values }, pos))
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
        encode_header(&mut buf);
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
        let entry = SchemaEntry {
            type_id: 1,
            name: "PollStart".into(),
            fields: vec![
                FieldDef { name: "ts".into(), field_type: FieldType::U64 },
                FieldDef { name: "worker".into(), field_type: FieldType::U64 },
            ],
        };
        let mut buf = Vec::new();
        encode_schema(&entry, &mut buf);
        assert_eq!(buf[0], TAG_SCHEMA);
        let (frame, consumed) = decode_frame(&buf, &|_| None).unwrap();
        assert_eq!(consumed, buf.len());
        assert_eq!(frame, Frame::Schema(entry));
    }

    #[test]
    fn schema_frame_empty_fields() {
        let entry = SchemaEntry { type_id: 0, name: "Empty".into(), fields: vec![] };
        let mut buf = Vec::new();
        encode_schema(&entry, &mut buf);
        let (frame, _) = decode_frame(&buf, &|_| None).unwrap();
        assert_eq!(frame, Frame::Schema(entry));
    }

    // --- Event frame tests ---

    #[test]
    fn event_frame_round_trip() {
        let values = vec![FieldValue::U64(12345), FieldValue::Bool(true), FieldValue::String(b"hi".to_vec())];
        let mut buf = Vec::new();
        encode_event(1, &values, &mut buf);
        assert_eq!(buf[0], TAG_EVENT);

        let types = vec![FieldType::U64, FieldType::Bool, FieldType::String];
        let lookup = |id: u16| -> Option<&[FieldType]> {
            if id == 1 { Some(&types) } else { None }
        };
        let (frame, consumed) = decode_frame(&buf, &lookup).unwrap();
        assert_eq!(consumed, buf.len());
        assert_eq!(frame, Frame::Event { type_id: 1, values });
    }

    #[test]
    fn event_frame_unknown_type_id() {
        let mut buf = Vec::new();
        encode_event(99, &[FieldValue::U64(1)], &mut buf);
        assert!(decode_frame(&buf, &|_| None).is_none());
    }

    #[test]
    fn event_frame_varint_compact() {
        // PollEnd with varint fields: tag(1) + type_id(2) + varint(1_050_000) + varint(3)
        let values = vec![FieldValue::Varint(1_050_000), FieldValue::Varint(3)];
        let mut buf = Vec::new();
        encode_event(2, &values, &mut buf);
        // 1 + 2 + 3 + 1 = 7 bytes (vs 23 before, vs 6 in old format)
        assert!(buf.len() <= 7, "varint PollEnd should be <=7 bytes, got {}", buf.len());

        let types = vec![FieldType::Varint, FieldType::Varint];
        let lookup = |id: u16| -> Option<&[FieldType]> {
            if id == 2 { Some(&types) } else { None }
        };
        let (frame, consumed) = decode_frame(&buf, &lookup).unwrap();
        assert_eq!(consumed, buf.len());
        assert_eq!(frame, Frame::Event { type_id: 2, values });
    }

    // --- String pool frame tests ---

    #[test]
    fn string_pool_round_trip() {
        let entries = vec![
            PoolEntry { pool_id: 0, data: b"main_thread".to_vec() },
            PoolEntry { pool_id: 1, data: b"worker-1".to_vec() },
        ];
        let mut buf = Vec::new();
        encode_string_pool(&entries, &mut buf);
        assert_eq!(buf[0], TAG_STRING_POOL);
        let (frame, consumed) = decode_frame(&buf, &|_| None).unwrap();
        assert_eq!(consumed, buf.len());
        assert_eq!(frame, Frame::StringPool(entries));
    }

    #[test]
    fn string_pool_empty() {
        let mut buf = Vec::new();
        encode_string_pool(&[], &mut buf);
        let (frame, _) = decode_frame(&buf, &|_| None).unwrap();
        assert_eq!(frame, Frame::StringPool(vec![]));
    }

    // --- Symbol table frame tests ---

    #[test]
    fn symbol_table_round_trip() {
        let entries = vec![
            SymbolEntry { base_addr: 0x1000, size: 256, symbol_id: 0 },
            SymbolEntry { base_addr: 0x2000, size: 128, symbol_id: 1 },
        ];
        let mut buf = Vec::new();
        encode_symbol_table(&entries, &mut buf);
        assert_eq!(buf[0], TAG_SYMBOL_TABLE);
        let (frame, consumed) = decode_frame(&buf, &|_| None).unwrap();
        assert_eq!(consumed, buf.len());
        assert_eq!(frame, Frame::SymbolTable(entries));
    }

    #[test]
    fn symbol_table_empty() {
        let mut buf = Vec::new();
        encode_symbol_table(&[], &mut buf);
        let (frame, _) = decode_frame(&buf, &|_| None).unwrap();
        assert_eq!(frame, Frame::SymbolTable(vec![]));
    }

    #[test]
    fn unknown_tag_returns_none() {
        assert!(decode_frame(&[0xFF], &|_| None).is_none());
    }
}
