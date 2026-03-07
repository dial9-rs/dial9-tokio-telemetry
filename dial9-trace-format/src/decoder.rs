// Streaming decoder

use crate::codec::{self, Frame, FrameRef, PoolEntry, PoolEntryRef, SymbolEntry, HEADER_SIZE};
use crate::schema::{SchemaEntry, SchemaRegistry};
use crate::types::{FieldType, FieldValueRef};
use std::collections::HashMap;

/// Decoded events yielded by the decoder.
#[derive(Debug, Clone, PartialEq)]
pub enum DecodedFrame {
    Schema(SchemaEntry),
    Event { type_id: u16, values: Vec<crate::types::FieldValue> },
    StringPool(Vec<PoolEntry>),
    SymbolTable(Vec<SymbolEntry>),
}

/// Zero-copy decoded frame that borrows from the input buffer.
#[derive(Debug, Clone, PartialEq)]
pub enum DecodedFrameRef<'a> {
    Schema(SchemaEntry),
    Event { type_id: u16, values: Vec<FieldValueRef<'a>> },
    StringPool(Vec<PoolEntryRef<'a>>),
    SymbolTable(Vec<SymbolEntry>),
}

pub struct Decoder<'a> {
    data: &'a [u8],
    pos: usize,
    registry: SchemaRegistry,
    /// Cached field types per type_id to avoid re-allocating on every event decode.
    field_types_cache: HashMap<u16, Vec<FieldType>>,
    pub string_pool: HashMap<u32, String>,
    pub version: u8,
}

impl<'a> Decoder<'a> {
    pub fn new(data: &'a [u8]) -> Option<Self> {
        let version = codec::decode_header(data)?;
        Some(Self { data, pos: HEADER_SIZE, registry: SchemaRegistry::new(), field_types_cache: HashMap::new(), string_pool: HashMap::new(), version })
    }

    pub fn registry(&self) -> &SchemaRegistry {
        &self.registry
    }

    /// Decode the next frame. Returns None when stream is exhausted.
    pub fn next_frame(&mut self) -> Option<DecodedFrame> {
        if self.pos >= self.data.len() {
            return None;
        }
        let remaining = &self.data[self.pos..];
        let cache = &self.field_types_cache;
        let lookup = |type_id: u16| -> Option<&[FieldType]> {
            cache.get(&type_id).map(|v| v.as_slice())
        };
        let (frame, consumed) = codec::decode_frame(remaining, &lookup)?;
        self.pos += consumed;
        match frame {
            Frame::Schema(entry) => {
                let result = DecodedFrame::Schema(entry.clone());
                self.field_types_cache.insert(entry.type_id, entry.fields.iter().map(|f| f.field_type).collect());
                let _ = self.registry.register(entry);
                Some(result)
            }
            Frame::Event { type_id, values } => Some(DecodedFrame::Event { type_id, values }),
            Frame::StringPool(entries) => {
                for e in &entries {
                    if let Ok(s) = String::from_utf8(e.data.clone()) {
                        self.string_pool.insert(e.pool_id, s);
                    }
                }
                Some(DecodedFrame::StringPool(entries))
            }
            Frame::SymbolTable(entries) => Some(DecodedFrame::SymbolTable(entries)),
        }
    }

    /// Collect all remaining frames.
    pub fn decode_all(&mut self) -> Vec<DecodedFrame> {
        let mut frames = Vec::new();
        while let Some(f) = self.next_frame() {
            frames.push(f);
        }
        frames
    }

    /// Decode the next frame without copying field data. Returns None when stream is exhausted.
    pub fn next_frame_ref(&mut self) -> Option<DecodedFrameRef<'a>> {
        if self.pos >= self.data.len() {
            return None;
        }
        let remaining = &self.data[self.pos..];
        let cache = &self.field_types_cache;
        let lookup = |type_id: u16| -> Option<&[FieldType]> {
            cache.get(&type_id).map(|v| v.as_slice())
        };
        let (frame, consumed) = codec::decode_frame_ref(remaining, &lookup)?;
        self.pos += consumed;
        match frame {
            FrameRef::Schema(entry) => {
                let result = DecodedFrameRef::Schema(entry.clone());
                self.field_types_cache.insert(entry.type_id, entry.fields.iter().map(|f| f.field_type).collect());
                let _ = self.registry.register(entry);
                Some(result)
            }
            FrameRef::Event { type_id, values } => Some(DecodedFrameRef::Event { type_id, values }),
            FrameRef::StringPool(entries) => {
                for e in &entries {
                    if let Ok(s) = std::str::from_utf8(e.data) {
                        self.string_pool.insert(e.pool_id, s.to_string());
                    }
                }
                Some(DecodedFrameRef::StringPool(entries))
            }
            FrameRef::SymbolTable(entries) => Some(DecodedFrameRef::SymbolTable(entries)),
        }
    }

    /// Collect all remaining frames using zero-copy decoding.
    pub fn decode_all_ref(&mut self) -> Vec<DecodedFrameRef<'a>> {
        let mut frames = Vec::new();
        while let Some(f) = self.next_frame_ref() {
            frames.push(f);
        }
        frames
    }

    /// Process all events with a callback, avoiding per-event Vec allocations.
    /// Schemas and string pools are registered automatically.
    /// The callback receives (type_id, &[FieldValueRef]).
    pub fn for_each_event(&mut self, mut f: impl FnMut(u16, &[FieldValueRef<'a>])) {
        let mut values_buf: Vec<FieldValueRef<'a>> = Vec::new();
        while self.pos < self.data.len() {
            let remaining = &self.data[self.pos..];
            let tag = match remaining.first() {
                Some(t) => *t,
                None => break,
            };
            match tag {
                codec::TAG_EVENT => {
                    let mut pos = 1;
                    let type_id = match remaining.get(pos..pos + 2) {
                        Some(b) => { pos += 2; u16::from_le_bytes(b.try_into().unwrap()) }
                        None => break,
                    };
                    let field_types = match self.field_types_cache.get(&type_id) {
                        Some(ft) => ft,
                        None => break,
                    };
                    values_buf.clear();
                    let mut ok = true;
                    for ft in field_types {
                        match FieldValueRef::decode(*ft, remaining, pos) {
                            Some((val, consumed)) => { values_buf.push(val); pos += consumed; }
                            None => { ok = false; break; }
                        }
                    }
                    if !ok { break; }
                    self.pos += pos;
                    f(type_id, &values_buf);
                }
                _ => {
                    // Use next_frame_ref for non-event frames (schema, pool, symbol table)
                    if self.next_frame_ref().is_none() { break; }
                }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::encoder::Encoder;
    use crate::schema::FieldDef;
    use crate::types::{FieldType, FieldValue};

    struct Ev;

    #[test]
    fn decode_empty_stream() {
        let enc = Encoder::new();
        let data = enc.finish();
        let mut dec = Decoder::new(&data).unwrap();
        assert_eq!(dec.version, 1);
        assert!(dec.next_frame().is_none());
    }

    #[test]
    fn decode_schema_frame() {
        let mut enc = Encoder::new();
        enc.register_schema_for::<Ev>("Ev", vec![
            FieldDef { name: "v".into(), field_type: FieldType::U64 },
        ]);
        let data = enc.finish();
        let mut dec = Decoder::new(&data).unwrap();
        let frame = dec.next_frame().unwrap();
        assert!(matches!(frame, DecodedFrame::Schema(s) if s.name == "Ev"));
    }

    #[test]
    fn decode_event_after_schema() {
        let mut enc = Encoder::new();
        enc.register_schema_for::<Ev>("Ev", vec![
            FieldDef { name: "v".into(), field_type: FieldType::U64 },
        ]);
        enc.write_event_for::<Ev>(&[FieldValue::U64(42)]);
        let data = enc.finish();

        let mut dec = Decoder::new(&data).unwrap();
        let frames = dec.decode_all();
        assert_eq!(frames.len(), 2);
        if let DecodedFrame::Event { values, .. } = &frames[1] {
            assert_eq!(*values, vec![FieldValue::U64(42)]);
        } else {
            panic!("expected event");
        }
    }

    #[test]
    fn decode_string_pool_builds_map() {
        let mut enc = Encoder::new();
        let id = enc.intern_string("hello");
        let data = enc.finish();

        let mut dec = Decoder::new(&data).unwrap();
        dec.decode_all();
        assert_eq!(dec.string_pool.get(&id.0), Some(&"hello".to_string()));
    }

    #[test]
    fn decode_multiple_events() {
        let mut enc = Encoder::new();
        enc.register_schema_for::<Ev>("Ev", vec![
            FieldDef { name: "v".into(), field_type: FieldType::U64 },
        ]);
        for i in 0..10u64 {
            enc.write_event_for::<Ev>(&[FieldValue::U64(i)]);
        }
        let data = enc.finish();

        let mut dec = Decoder::new(&data).unwrap();
        let frames = dec.decode_all();
        assert_eq!(frames.len(), 11);
    }

    #[test]
    fn bad_header_returns_none() {
        assert!(Decoder::new(&[0x00, 0x00, 0x00, 0x00, 1]).is_none());
    }
}
