// Streaming decoder

use crate::codec::{
    self, Frame, FrameRef, HEADER_SIZE, PoolEntry, PoolEntryRef, SchemaInfo, SymbolEntry,
    WireTypeId,
};
use crate::schema::{SchemaEntry, SchemaRegistry};
use crate::types::{FieldType, FieldValueRef, InternedString};
use std::collections::HashMap;
use std::fmt;

/// Error returned when the decoder cannot continue reading the stream.
/// Because frames are not length-prefixed, a decode error is unrecoverable —
/// the decoder cannot skip the malformed frame to find the next one.
#[derive(Debug, Clone)]
pub struct DecodeError {
    pub pos: usize,
    pub message: String,
}

impl fmt::Display for DecodeError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "decode error at byte {}: {}", self.pos, self.message)
    }
}

impl std::error::Error for DecodeError {}

/// A decoded event passed to [`Decoder::for_each_event`].
///
/// `'a` is the lifetime of the input data buffer (strings, stack frames borrow from it).
/// `'f` is the lifetime of the `fields` slice and schema name (reused across calls).
#[non_exhaustive]
pub struct RawEvent<'a, 'f> {
    pub type_id: WireTypeId,
    pub name: &'f str,
    pub timestamp_ns: Option<u64>,
    pub fields: &'f [FieldValueRef<'a>],
    pub string_pool: &'f StringPool,
}

/// A map from interned string IDs to their resolved string values.
///
/// Populated automatically by the [`Decoder`] as it processes `StringPool` frames.
/// Pass a reference to [`crate::TraceEvent::decode`] so that `InternedString` fields
/// resolve to `&str` in derived `Ref` types.
#[derive(Debug, Clone, Default)]
pub struct StringPool(HashMap<InternedString, String>);

impl StringPool {
    pub(crate) fn new() -> Self {
        Self(HashMap::new())
    }

    pub(crate) fn insert(&mut self, id: InternedString, value: String) {
        self.0.insert(id, value);
    }

    pub fn get(&self, id: InternedString) -> Option<&str> {
        self.0.get(&id).map(|s| s.as_str())
    }

    pub fn len(&self) -> usize {
        self.0.len()
    }

    pub fn is_empty(&self) -> bool {
        self.0.is_empty()
    }
}

/// Decoded events yielded by the decoder.
#[derive(Debug, Clone, PartialEq)]
pub enum DecodedFrame {
    Schema(SchemaEntry),
    Event {
        type_id: WireTypeId,
        /// Absolute timestamp in nanoseconds, if the schema has `has_timestamp`.
        timestamp_ns: Option<u64>,
        values: Vec<crate::types::FieldValue>,
    },
    StringPool(Vec<PoolEntry>),
    SymbolTable(Vec<SymbolEntry>),
}

/// Zero-copy decoded frame that borrows from the input buffer.
#[derive(Debug, Clone, PartialEq)]
pub enum DecodedFrameRef<'a> {
    Schema(SchemaEntry),
    Event {
        type_id: WireTypeId,
        timestamp_ns: Option<u64>,
        values: Vec<FieldValueRef<'a>>,
    },
    StringPool(Vec<PoolEntryRef<'a>>),
    SymbolTable(Vec<SymbolEntry>),
}

struct SchemaCache {
    name: String,
    field_types: Vec<FieldType>,
    has_timestamp: bool,
}

pub struct Decoder<'a> {
    data: &'a [u8],
    pos: usize,
    registry: SchemaRegistry,
    schema_cache: HashMap<WireTypeId, SchemaCache>,
    string_pool: StringPool,
    version: u8,
    timestamp_base_ns: u64,
}

impl<'a> Decoder<'a> {
    pub fn new(data: &'a [u8]) -> Option<Self> {
        let version = codec::decode_header(data)?;
        Some(Self {
            data,
            pos: HEADER_SIZE,
            registry: SchemaRegistry::new(),
            schema_cache: HashMap::new(),
            string_pool: StringPool::new(),
            version,
            timestamp_base_ns: 0,
        })
    }

    pub fn registry(&self) -> &SchemaRegistry {
        &self.registry
    }

    pub fn version(&self) -> u8 {
        self.version
    }

    pub fn string_pool(&self) -> &StringPool {
        &self.string_pool
    }

    fn schema_info(&self, type_id: WireTypeId) -> Option<SchemaInfo<'_>> {
        self.schema_cache.get(&type_id).map(|c| SchemaInfo {
            field_types: &c.field_types,
            has_timestamp: c.has_timestamp,
        })
    }

    fn register_schema(&mut self, type_id: WireTypeId, entry: SchemaEntry) -> Result<(), String> {
        self.schema_cache.insert(
            type_id,
            SchemaCache {
                name: entry.name.clone(),
                field_types: entry.fields.iter().map(|f| f.field_type).collect(),
                has_timestamp: entry.has_timestamp,
            },
        );
        self.registry.register(type_id, entry)
    }

    /// Decode the next frame. Returns `Ok(None)` when stream is exhausted.
    /// Returns `Err` if the stream is malformed (e.g. duplicate type_id with
    /// a different schema).
    pub fn next_frame(&mut self) -> Result<Option<DecodedFrame>, DecodeError> {
        if self.pos >= self.data.len() {
            return Ok(None);
        }
        let remaining = &self.data[self.pos..];
        let base = self.timestamp_base_ns;
        let (frame, consumed) =
            match codec::decode_frame(remaining, |type_id| self.schema_info(type_id), base) {
                Some(r) => r,
                None => return Ok(None),
            };
        self.pos += consumed;
        match frame {
            Frame::Schema { type_id, entry } => {
                let result = DecodedFrame::Schema(entry.clone());
                self.register_schema(type_id, entry)
                    .map_err(|msg| DecodeError {
                        pos: self.pos,
                        message: msg,
                    })?;
                Ok(Some(result))
            }
            Frame::Event {
                type_id,
                timestamp_ns,
                values,
            } => {
                if let Some(ts) = timestamp_ns {
                    self.timestamp_base_ns = ts;
                }
                Ok(Some(DecodedFrame::Event {
                    type_id,
                    timestamp_ns,
                    values,
                }))
            }
            Frame::StringPool(entries) => {
                for e in &entries {
                    if let Ok(s) = String::from_utf8(e.data.clone()) {
                        self.string_pool.insert(InternedString(e.pool_id), s);
                    }
                }
                Ok(Some(DecodedFrame::StringPool(entries)))
            }
            Frame::SymbolTable(entries) => Ok(Some(DecodedFrame::SymbolTable(entries))),
            Frame::TimestampReset(ts) => {
                self.timestamp_base_ns = ts;
                self.next_frame() // consume silently, return next real frame
            }
        }
    }

    /// Collect all remaining frames. Stops on error or end of stream.
    pub fn decode_all(&mut self) -> Vec<DecodedFrame> {
        let mut frames = Vec::new();
        while let Ok(Some(f)) = self.next_frame() {
            frames.push(f);
        }
        frames
    }

    /// Decode the next frame without copying field data. Returns `Ok(None)` when
    /// stream is exhausted. Returns `Err` on malformed data.
    pub fn next_frame_ref(&mut self) -> Result<Option<DecodedFrameRef<'a>>, DecodeError> {
        if self.pos >= self.data.len() {
            return Ok(None);
        }
        let remaining = &self.data[self.pos..];
        let base = self.timestamp_base_ns;
        let (frame, consumed) =
            match codec::decode_frame_ref(remaining, |type_id| self.schema_info(type_id), base) {
                Some(r) => r,
                None => return Ok(None),
            };
        self.pos += consumed;
        match frame {
            FrameRef::Schema { type_id, entry } => {
                let result = DecodedFrameRef::Schema(entry.clone());
                self.register_schema(type_id, entry)
                    .map_err(|msg| DecodeError {
                        pos: self.pos,
                        message: msg,
                    })?;
                Ok(Some(result))
            }
            FrameRef::Event {
                type_id,
                timestamp_ns,
                values,
            } => {
                if let Some(ts) = timestamp_ns {
                    self.timestamp_base_ns = ts;
                }
                Ok(Some(DecodedFrameRef::Event {
                    type_id,
                    timestamp_ns,
                    values,
                }))
            }
            FrameRef::StringPool(entries) => {
                for e in &entries {
                    if let Ok(s) = std::str::from_utf8(e.data) {
                        self.string_pool
                            .insert(InternedString(e.pool_id), s.to_string());
                    }
                }
                Ok(Some(DecodedFrameRef::StringPool(entries)))
            }
            FrameRef::SymbolTable(entries) => Ok(Some(DecodedFrameRef::SymbolTable(entries))),
            FrameRef::TimestampReset(ts) => {
                self.timestamp_base_ns = ts;
                self.next_frame_ref()
            }
        }
    }

    /// Collect all remaining frames using zero-copy decoding. Stops on error or end of stream.
    pub fn decode_all_ref(&mut self) -> Vec<DecodedFrameRef<'a>> {
        let mut frames = Vec::new();
        while let Ok(Some(f)) = self.next_frame_ref() {
            frames.push(f);
        }
        frames
    }

    /// Process all events with a callback, avoiding per-event Vec allocations.
    /// Schemas and string pools are registered automatically.
    ///
    /// The [`RawEvent`] passed to the callback borrows from the decoder's input
    /// buffer. The `fields` slice is reused across calls, so values cannot be
    /// stored across iterations without copying.
    ///
    /// Returns `Err` if the stream is malformed.
    pub fn for_each_event(
        &mut self,
        mut f: impl for<'f> FnMut(RawEvent<'a, 'f>),
    ) -> Result<(), DecodeError> {
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
                        Some(b) => {
                            pos += 2;
                            WireTypeId(u16::from_le_bytes(b.try_into().unwrap()))
                        }
                        None => {
                            return Err(DecodeError {
                                pos: self.pos,
                                message: "truncated event frame".into(),
                            });
                        }
                    };
                    let cache = match self.schema_cache.get(&type_id) {
                        Some(c) => c,
                        None => {
                            return Err(DecodeError {
                                pos: self.pos,
                                message: format!("unknown type_id {type_id:?}"),
                            });
                        }
                    };

                    let timestamp_ns = if cache.has_timestamp {
                        match codec::decode_u24_le(&remaining[pos..]) {
                            Some(delta) => {
                                pos += 3;
                                Some(self.timestamp_base_ns + delta as u64)
                            }
                            None => {
                                return Err(DecodeError {
                                    pos: self.pos + pos,
                                    message: "truncated timestamp delta".into(),
                                });
                            }
                        }
                    } else {
                        None
                    };

                    values_buf.clear();
                    for ft in &cache.field_types {
                        match FieldValueRef::decode(*ft, remaining, pos) {
                            Some((val, consumed)) => {
                                values_buf.push(val);
                                pos += consumed;
                            }
                            None => {
                                return Err(DecodeError {
                                    pos: self.pos + pos,
                                    message: "truncated field value".into(),
                                });
                            }
                        }
                    }
                    self.pos += pos;
                    if let Some(ts) = timestamp_ns {
                        self.timestamp_base_ns = ts;
                    }
                    f(RawEvent {
                        type_id,
                        name: &cache.name,
                        timestamp_ns,
                        fields: &values_buf,
                        string_pool: &self.string_pool,
                    });
                }
                codec::TAG_TIMESTAMP_RESET => {
                    // Handle timestamp resets inline to avoid next_frame_ref's
                    // recursive consumption of the following frame.
                    let ts = match self.data.get(self.pos + 1..self.pos + 9) {
                        Some(b) => u64::from_le_bytes(b.try_into().unwrap()),
                        None => {
                            return Err(DecodeError {
                                pos: self.pos,
                                message: "truncated timestamp reset".into(),
                            });
                        }
                    };
                    self.timestamp_base_ns = ts;
                    self.pos += 9;
                }
                _ => {
                    // Use next_frame_ref for non-event frames (schema, pool, symbol table)
                    // `next_frame_ref` will update the decoder state as we read the frames (e.g. the pooled strings)
                    match self.next_frame_ref() {
                        Ok(Some(_)) => {}
                        Ok(None) => {
                            return Err(DecodeError {
                                pos: self.pos,
                                message: format!("failed to decode frame with tag 0x{tag:02x}"),
                            });
                        }
                        Err(e) => return Err(e),
                    }
                }
            }
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::encoder::Encoder;
    use crate::schema::FieldDef;
    use crate::types::{FieldType, FieldValue};

    #[test]
    fn decode_empty_stream() {
        let enc = Encoder::new();
        let data = enc.finish();
        let mut dec = Decoder::new(&data).unwrap();
        assert_eq!(dec.version(), 1);
        assert!(dec.next_frame().unwrap().is_none());
    }

    #[test]
    fn decode_schema_frame() {
        let mut enc = Encoder::new();
        enc.register_schema(
            "Ev",
            vec![FieldDef {
                name: "v".into(),
                field_type: FieldType::Varint,
            }],
        )
        .unwrap();
        let data = enc.finish();
        let mut dec = Decoder::new(&data).unwrap();
        let frame = dec.next_frame().unwrap().unwrap();
        assert!(matches!(frame, DecodedFrame::Schema(s) if s.name == "Ev"));
    }

    #[test]
    fn decode_event_after_schema() {
        let mut enc = Encoder::new();
        let schema = enc
            .register_schema(
                "Ev",
                vec![FieldDef {
                    name: "v".into(),
                    field_type: FieldType::Varint,
                }],
            )
            .unwrap();
        enc.write_event(
            &schema,
            &[FieldValue::Varint(1_000), FieldValue::Varint(42)],
        )
        .unwrap();
        let data = enc.finish();

        let mut dec = Decoder::new(&data).unwrap();
        let frames = dec.decode_all();
        assert_eq!(frames.len(), 2);
        if let DecodedFrame::Event { values, .. } = &frames[1] {
            assert_eq!(*values, vec![FieldValue::Varint(42)]);
        } else {
            panic!("expected event");
        }
    }

    #[test]
    fn decode_string_pool_builds_map() {
        let mut enc = Encoder::new();
        let id = enc.intern_string("hello").unwrap();
        let data = enc.finish();

        let mut dec = Decoder::new(&data).unwrap();
        dec.decode_all();
        assert_eq!(dec.string_pool().get(id), Some("hello"));
    }

    #[test]
    fn decode_multiple_events() {
        let mut enc = Encoder::new();
        let schema = enc
            .register_schema(
                "Ev",
                vec![FieldDef {
                    name: "v".into(),
                    field_type: FieldType::Varint,
                }],
            )
            .unwrap();
        for i in 0..10u64 {
            enc.write_event(
                &schema,
                &[FieldValue::Varint(i * 1000), FieldValue::Varint(i)],
            )
            .unwrap();
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
