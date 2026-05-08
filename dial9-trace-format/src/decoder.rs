//! Streaming decoder for reading trace files.
//!
//! [`Decoder`] reads the file header, processes schema and string-pool frames,
//! and yields events as [`DecodedFrame`] (owned) or [`DecodedFrameRef`]
//! (zero-copy). It also implements [`Iterator`] and provides a
//! [`for_each_event`](Decoder::for_each_event) callback API for
//! allocation-free processing.

use crate::codec::{
    self, Frame, FrameRef, HEADER_SIZE, PoolEntry, PoolEntryRef, SchemaInfo, StackPoolEntry,
    StackPoolEntryRef, WireTypeId,
};
use crate::schema::{SchemaEntry, SchemaRegistry};
use crate::types::{FieldType, FieldValueRef, InternedStackFrames, InternedString, StackFrames};
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

/// Error returned by [`Decoder::try_for_each_event`].
#[derive(Debug)]
pub enum TryForEachError<E> {
    Decode(DecodeError),
    User(E),
}

impl<E: fmt::Display> fmt::Display for TryForEachError<E> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            TryForEachError::Decode(e) => write!(f, "{e}"),
            TryForEachError::User(e) => write!(f, "{e}"),
        }
    }
}

impl<E: fmt::Display + fmt::Debug> std::error::Error for TryForEachError<E> {}

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
    pub schema: &'f SchemaEntry,
    pub string_pool: &'f StringPool,
    pub stack_pool: &'f StackPool,
}

impl<'a, 'f> RawEvent<'a, 'f> {
    /// Field names from the schema, parallel to `fields`.
    pub fn field_names(&self) -> impl Iterator<Item = &'f str> {
        self.schema.fields.iter().map(|f| f.name.as_str())
    }
}

/// A map from interned string IDs to their resolved string values.
///
/// Populated automatically by the [`Decoder`] as it processes `StringPool` frames.
/// Pass a reference to [`crate::TraceEvent::decode`] so that `InternedString` fields
/// resolve to `&str` in derived `Ref` types.
#[derive(Debug, Default)]
pub struct StringPool(pub(crate) HashMap<InternedString, String>);

impl StringPool {
    pub(crate) fn new() -> Self {
        Self(HashMap::default())
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

    /// Iterate over all interned strings as `(id, value)` pairs.
    pub fn iter(&self) -> impl Iterator<Item = (InternedString, &str)> {
        self.0.iter().map(|(&id, v)| (id, v.as_str()))
    }
}

/// A map from interned stack-frame IDs to their resolved address vectors.
///
/// Populated automatically by the [`Decoder`] as it processes `StackPool` frames.
#[derive(Debug, Default)]
pub struct StackPool(pub(crate) HashMap<InternedStackFrames, Vec<u64>>);

impl StackPool {
    pub(crate) fn new() -> Self {
        Self(HashMap::default())
    }

    pub(crate) fn insert(&mut self, id: InternedStackFrames, frames: StackFrames) {
        self.0.insert(id, frames.0);
    }

    pub fn get(&self, id: InternedStackFrames) -> Option<&[u64]> {
        self.0.get(&id).map(|v| v.as_slice())
    }

    pub fn len(&self) -> usize {
        self.0.len()
    }

    pub fn is_empty(&self) -> bool {
        self.0.is_empty()
    }

    /// Iterate over all interned stack frames as `(id, frames)` pairs.
    pub fn iter(&self) -> impl Iterator<Item = (InternedStackFrames, &[u64])> {
        self.0.iter().map(|(&id, v)| (id, v.as_slice()))
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
    StackPool(Vec<StackPoolEntry>),
    SchemaAnnotations {
        type_id: WireTypeId,
        annotations: Vec<crate::schema::FieldAnnotation>,
    },
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
    StackPool(Vec<StackPoolEntryRef<'a>>),
    SchemaAnnotations {
        type_id: WireTypeId,
        annotations: Vec<crate::schema::FieldAnnotation>,
    },
}

struct SchemaCache {
    entry: SchemaEntry,
    /// Raw field type tags for fast decode (avoids re-extracting from entry.fields).
    field_tags: Vec<u8>,
}

/// Streaming trace file decoder.
///
/// Reads from a byte slice, processing schema, string-pool, and event frames.
/// Implements [`Iterator`] over [`DecodedFrameRef`] for convenient consumption.
pub struct Decoder<'a> {
    data: &'a [u8],
    pos: usize,
    registry: SchemaRegistry,
    schema_cache: Vec<Option<SchemaCache>>,
    string_pool: StringPool,
    stack_pool: StackPool,
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
            schema_cache: Vec::new(),
            string_pool: StringPool::new(),
            stack_pool: StackPool::new(),
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

    pub fn stack_pool(&self) -> &StackPool {
        &self.stack_pool
    }

    /// Reset decoder state (schemas, string pool, timestamp base) as if
    /// starting a fresh stream. Used when a mid-stream header is encountered
    /// (the "reset frame" pattern for concatenated thread-local batches).
    fn reset_state(&mut self) {
        self.registry = SchemaRegistry::new();
        self.schema_cache.clear();
        self.string_pool = StringPool::new();
        self.stack_pool = StackPool::new();
        self.timestamp_base_ns = 0;
    }

    /// If the current position starts with a valid header, reset state and
    /// skip past it, returning true.
    fn try_consume_reset_header(&mut self) -> bool {
        if self.pos + HEADER_SIZE <= self.data.len()
            && codec::decode_header(&self.data[self.pos..]).is_some()
        {
            self.reset_state();
            self.pos += HEADER_SIZE;
            true
        } else {
            false
        }
    }

    /// Consume this decoder and create an [`Encoder`](crate::encoder::Encoder) that appends to the
    /// decoded trace. The encoder inherits the string pool, schema registry,
    /// and timestamp base so new frames are compatible with the existing data.
    ///
    /// No file header is written — the caller is responsible for concatenating
    /// the encoder's output after the original trace bytes.
    pub fn into_encoder<W: std::io::Write>(self, writer: W) -> crate::encoder::Encoder<W> {
        crate::encoder::Encoder::from_decoder(
            self.registry,
            self.string_pool,
            self.stack_pool,
            self.timestamp_base_ns,
            writer,
        )
    }

    pub(crate) fn schema_info(&self, type_id: WireTypeId) -> Option<SchemaInfo<'_>> {
        self.schema_cache
            .get(type_id.0 as usize)
            .and_then(|s| s.as_ref())
            .map(|c| SchemaInfo {
                field_tags: &c.field_tags,
                has_timestamp: c.entry.has_timestamp,
            })
    }

    fn register_schema(&mut self, type_id: WireTypeId, entry: SchemaEntry) -> Result<(), String> {
        let idx = type_id.0 as usize;
        if idx >= self.schema_cache.len() {
            self.schema_cache.resize_with(idx + 1, || None);
        }
        self.schema_cache[idx] = Some(SchemaCache {
            field_tags: entry.fields.iter().map(|f| f.field_type as u8).collect(),
            entry: entry.clone(),
        });
        self.registry.register(type_id, entry)
    }

    /// Decode the next frame. Returns `Ok(None)` when stream is exhausted.
    /// Returns `Err` if the stream is malformed (e.g. duplicate type_id with
    /// a different schema).
    pub fn next_frame(&mut self) -> Result<Option<DecodedFrame>, DecodeError> {
        if self.pos >= self.data.len() {
            return Ok(None);
        }
        if self.try_consume_reset_header() {
            return self.next_frame();
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
            Frame::StackPool(entries) => {
                for e in &entries {
                    self.stack_pool
                        .insert(InternedStackFrames(e.pool_id), e.frames.clone().into());
                }
                Ok(Some(DecodedFrame::StackPool(entries)))
            }
            Frame::TimestampReset(ts) => {
                self.timestamp_base_ns = ts;
                self.next_frame() // consume silently, return next real frame
            }
            Frame::SchemaAnnotations {
                type_id,
                annotations,
            } => {
                // Merge annotations into the cached schema (lenient: skip if unknown type_id)
                if let Some(cache) = self
                    .schema_cache
                    .get_mut(type_id.0 as usize)
                    .and_then(|s| s.as_mut())
                {
                    cache.entry.annotations.extend_from_slice(&annotations);
                }
                if let Some(entry) = self.registry.schemas.get_mut(&type_id) {
                    entry.annotations.extend_from_slice(&annotations);
                }
                Ok(Some(DecodedFrame::SchemaAnnotations {
                    type_id,
                    annotations,
                }))
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
        if self.try_consume_reset_header() {
            return self.next_frame_ref();
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
            FrameRef::StackPool(entries) => {
                for e in &entries {
                    self.stack_pool
                        .insert(InternedStackFrames(e.pool_id), e.to_stack_frames());
                }
                Ok(Some(DecodedFrameRef::StackPool(entries)))
            }
            FrameRef::TimestampReset(ts) => {
                self.timestamp_base_ns = ts;
                self.next_frame_ref()
            }
            FrameRef::SchemaAnnotations {
                type_id,
                annotations,
            } => {
                if let Some(cache) = self
                    .schema_cache
                    .get_mut(type_id.0 as usize)
                    .and_then(|s| s.as_mut())
                {
                    cache.entry.annotations.extend_from_slice(&annotations);
                }
                if let Some(entry) = self.registry.schemas.get_mut(&type_id) {
                    entry.annotations.extend_from_slice(&annotations);
                }
                Ok(Some(DecodedFrameRef::SchemaAnnotations {
                    type_id,
                    annotations,
                }))
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
        self.try_for_each_event(|ev| {
            f(ev);
            Ok::<(), std::convert::Infallible>(())
        })
        .map_err(|e| match e {
            TryForEachError::Decode(d) => d,
            TryForEachError::User(inf) => match inf {},
        })
    }

    /// Like [`for_each_event`](Self::for_each_event), but the callback may
    /// return an error to stop iteration early.
    pub fn try_for_each_event<E>(
        &mut self,
        mut f: impl for<'f> FnMut(RawEvent<'a, 'f>) -> Result<(), E>,
    ) -> Result<(), TryForEachError<E>> {
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
                            return Err(TryForEachError::Decode(DecodeError {
                                pos: self.pos,
                                message: "truncated event frame".into(),
                            }));
                        }
                    };
                    let cache = match self
                        .schema_cache
                        .get(type_id.0 as usize)
                        .and_then(|s| s.as_ref())
                    {
                        Some(c) => c,
                        None => {
                            return Err(TryForEachError::Decode(DecodeError {
                                pos: self.pos,
                                message: format!("unknown type_id {type_id:?}"),
                            }));
                        }
                    };

                    let timestamp_ns = if cache.entry.has_timestamp {
                        match codec::decode_u24_le(&remaining[pos..]) {
                            Some(delta) => {
                                pos += 3;
                                Some(self.timestamp_base_ns + delta as u64)
                            }
                            None => {
                                return Err(TryForEachError::Decode(DecodeError {
                                    pos: self.pos + pos,
                                    message: "truncated timestamp delta".into(),
                                }));
                            }
                        }
                    } else {
                        None
                    };

                    values_buf.clear();
                    for &ftag in &cache.field_tags {
                        let inner_type = match FieldType::from_tag(ftag) {
                            Some(ft) => ft,
                            None => {
                                return Err(TryForEachError::Decode(DecodeError {
                                    pos: self.pos + pos,
                                    message: format!("unknown field type tag {ftag:#x}"),
                                }));
                            }
                        };
                        if inner_type.is_optional() {
                            match remaining.get(pos) {
                                Some(0x00) => {
                                    values_buf.push(FieldValueRef::None);
                                    pos += 1;
                                }
                                Some(_) => {
                                    pos += 1;
                                    match FieldValueRef::decode(inner_type.inner(), remaining, pos)
                                    {
                                        Some((val, consumed)) => {
                                            values_buf.push(val);
                                            pos += consumed;
                                        }
                                        None => {
                                            return Err(TryForEachError::Decode(DecodeError {
                                                pos: self.pos + pos,
                                                message: "truncated optional field value".into(),
                                            }));
                                        }
                                    }
                                }
                                None => {
                                    return Err(TryForEachError::Decode(DecodeError {
                                        pos: self.pos + pos,
                                        message: "truncated optional field prefix".into(),
                                    }));
                                }
                            }
                        } else {
                            match FieldValueRef::decode(inner_type, remaining, pos) {
                                Some((val, consumed)) => {
                                    values_buf.push(val);
                                    pos += consumed;
                                }
                                None => {
                                    return Err(TryForEachError::Decode(DecodeError {
                                        pos: self.pos + pos,
                                        message: "truncated field value".into(),
                                    }));
                                }
                            }
                        }
                    }
                    // Update mutable state. The borrow checker allows this
                    // because `cache` borrows `self.schema_cache` while we
                    // mutate `self.pos` and `self.timestamp_base_ns`, which
                    // are disjoint fields. We use a block with destructured
                    // refs to make this explicit.
                    {
                        let Self {
                            pos: self_pos,
                            timestamp_base_ns,
                            ..
                        } = self;
                        *self_pos += pos;
                        if let Some(ts) = timestamp_ns {
                            *timestamp_base_ns = ts;
                        }
                    }
                    f(RawEvent {
                        type_id,
                        name: &cache.entry.name,
                        timestamp_ns,
                        fields: &values_buf,
                        schema: &cache.entry,
                        string_pool: &self.string_pool,
                        stack_pool: &self.stack_pool,
                    })
                    .map_err(TryForEachError::User)?;
                }
                codec::TAG_TIMESTAMP_RESET => {
                    let ts = match self.data.get(self.pos + 1..self.pos + 9) {
                        Some(b) => u64::from_le_bytes(b.try_into().unwrap()),
                        None => {
                            return Err(TryForEachError::Decode(DecodeError {
                                pos: self.pos,
                                message: "truncated timestamp reset".into(),
                            }));
                        }
                    };
                    self.timestamp_base_ns = ts;
                    self.pos += 9;
                }
                _ => {
                    // Mid-stream header = reset frame (tag 0x54 = 'T' from TRC\0)
                    if tag == codec::MAGIC[0] && self.try_consume_reset_header() {
                        continue;
                    }
                    match self.next_frame_ref() {
                        Ok(Some(_)) => {}
                        Ok(None) => {
                            return Err(TryForEachError::Decode(DecodeError {
                                pos: self.pos,
                                message: format!("failed to decode frame with tag 0x{tag:02x}"),
                            }));
                        }
                        Err(e) => return Err(TryForEachError::Decode(e)),
                    }
                }
            }
        }
        Ok(())
    }

    /// Returns an iterator that yields only [`DecodedFrameRef::Event`] variants,
    /// silently consuming schema, string-pool, and symbol-table frames
    /// (while still updating internal decoder state).
    pub fn events(&mut self) -> EventIter<'_, 'a> {
        EventIter { decoder: self }
    }
}

impl<'a> Iterator for Decoder<'a> {
    type Item = Result<DecodedFrameRef<'a>, DecodeError>;

    fn next(&mut self) -> Option<Self::Item> {
        self.next_frame_ref().transpose()
    }
}

/// Iterator that yields only [`DecodedFrameRef::Event`] frames,
/// consuming non-event frames to keep decoder state up to date.
pub struct EventIter<'d, 'a> {
    decoder: &'d mut Decoder<'a>,
}

impl<'d, 'a> Iterator for EventIter<'d, 'a> {
    type Item = Result<DecodedFrameRef<'a>, DecodeError>;

    fn next(&mut self) -> Option<Self::Item> {
        loop {
            match self.decoder.next()? {
                Ok(frame @ DecodedFrameRef::Event { .. }) => return Some(Ok(frame)),
                Ok(_) => continue, // schema, string pool, symbol table — skip
                Err(e) => return Some(Err(e)),
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

    #[test]
    fn iterator_yields_all_frames() {
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
        for i in 0..3u64 {
            enc.write_event(
                &schema,
                &[FieldValue::Varint(i * 1000), FieldValue::Varint(i)],
            )
            .unwrap();
        }
        let data = enc.finish();

        let dec = Decoder::new(&data).unwrap();
        let frames: Vec<_> = dec.collect::<Result<Vec<_>, _>>().unwrap();
        // 1 schema + 3 events
        assert_eq!(frames.len(), 4);
        assert!(matches!(frames[0], DecodedFrameRef::Schema(_)));
        assert!(matches!(frames[1], DecodedFrameRef::Event { .. }));
    }

    #[test]
    fn iterator_early_termination() {
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
        // Take just 2 frames (schema + first event), don't decode the rest
        let first_two: Vec<_> = dec.by_ref().take(2).collect::<Result<Vec<_>, _>>().unwrap();
        assert_eq!(first_two.len(), 2);
        // Decoder should still have remaining data
        let next = dec.next();
        assert!(next.is_some());
    }

    #[test]
    fn events_iterator_skips_schema() {
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
        enc.write_event(
            &schema,
            &[FieldValue::Varint(2_000), FieldValue::Varint(99)],
        )
        .unwrap();
        let data = enc.finish();

        let mut dec = Decoder::new(&data).unwrap();
        let events: Vec<_> = dec.events().collect::<Result<Vec<_>, _>>().unwrap();
        // Only events, no schema frame
        assert_eq!(events.len(), 2);
        for ev in &events {
            assert!(matches!(ev, DecodedFrameRef::Event { .. }));
        }
    }

    #[test]
    fn events_iterator_first_event_only() {
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
        for i in 0..5u64 {
            enc.write_event(
                &schema,
                &[FieldValue::Varint(i * 1000), FieldValue::Varint(i)],
            )
            .unwrap();
        }
        let data = enc.finish();

        let mut dec = Decoder::new(&data).unwrap();
        // Get just the first event — schema is consumed internally
        let first = dec.events().next().unwrap().unwrap();
        assert!(matches!(first, DecodedFrameRef::Event { .. }));
    }
}
