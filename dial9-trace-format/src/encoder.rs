//! High-level encoder for writing trace files.
//!
//! [`Encoder`] writes the file header, registers schemas, interns strings, and
//! encodes events with delta-compressed timestamps. It is the primary entry
//! point for producing trace data.

use crate::TraceEvent;
use crate::codec::{self, PoolEntry, WireTypeId};
use crate::schema::{SchemaEntry, SchemaRegistry};
use crate::types::{CountingWriter, EncodeState, EventEncoder, InternedString};
use std::any::TypeId;
use std::collections::HashMap;
use std::hash::{BuildHasherDefault, Hasher};
use std::io::{self, Write};
use std::sync::Arc;

/// A fast, non-cryptographic hasher using FxHash's multiply-shift strategy.
///
/// For HashMap keys that are already well-distributed (TypeId, Arc<str>), this
/// avoids hash collisions.
#[derive(Default)]
pub(crate) struct FxHasher(u64);

impl Hasher for FxHasher {
    #[inline]
    fn write(&mut self, bytes: &[u8]) {
        for &b in bytes {
            self.0 = (self.0.rotate_left(5) ^ b as u64).wrapping_mul(0x517cc1b727220a95);
        }
    }

    #[inline]
    fn write_u64(&mut self, i: u64) {
        self.0 = (self.0.rotate_left(5) ^ i).wrapping_mul(0x517cc1b727220a95);
    }

    #[inline]
    fn write_u32(&mut self, i: u32) {
        self.write_u64(i as u64)
    }

    #[inline]
    fn write_u16(&mut self, i: u16) {
        self.write_u64(i as u64)
    }

    #[inline]
    fn write_usize(&mut self, i: usize) {
        self.write_u64(i as u64)
    }

    #[inline]
    fn write_u128(&mut self, i: u128) {
        self.write_u64(i as u64);
        self.write_u64((i >> 64) as u64);
    }

    #[inline]
    fn finish(&self) -> u64 {
        self.0
    }
}

pub(crate) type FxBuildHasher = BuildHasherDefault<FxHasher>;
pub(crate) type FxHashMap<K, V> = HashMap<K, V, FxBuildHasher>;

/// A schema handle returned by [`Encoder::register_schema`] or created via
/// [`Schema::new`].
///
/// Carries the full schema definition (name + fields) so it can auto-register
/// itself with any encoder on first use. This means a `Schema` created on one
/// encoder can be passed to a different encoder and it will just work.
///
/// `Schema` is cheap to clone (internally `Arc`-backed).
#[derive(Clone, Debug)]
pub struct Schema {
    pub(crate) entry: Arc<SchemaEntry>,
    /// Pre-computed `Arc<str>` of the schema name, used as a cheap HashMap key
    /// (clone is a pointer bump instead of a String allocation).
    name_key: Arc<str>,
}

impl Schema {
    /// Create a schema handle without an encoder.
    ///
    /// The schema will be lazily registered the first time it is passed to
    /// [`Encoder::write_event`].
    pub fn new(name: &str, fields: Vec<crate::schema::FieldDef>) -> Self {
        let name_key: Arc<str> = Arc::from(name);
        Self {
            entry: Arc::new(SchemaEntry {
                name: name.to_string(),
                has_timestamp: true,
                fields,
            }),
            name_key,
        }
    }

    /// Schema name.
    pub fn name(&self) -> &str {
        &self.entry.name
    }

    /// Schema field definitions.
    pub fn fields(&self) -> &[crate::schema::FieldDef] {
        &self.entry.fields
    }
}

/// Key for schema lookup — either by name (manual registration) or by Rust
/// `TypeId` (derive macro path).
#[derive(Clone, PartialEq, Eq, Hash)]
enum SchemaKey {
    Name(Arc<str>),
    RustType(TypeId),
}

/// Trace file encoder.
///
/// Writes the binary file header, registers event schemas, interns strings
/// into a pool, and encodes events with delta-compressed timestamps.
///
/// The default type parameter (`Vec<u8>`) buffers everything in memory;
/// use [`Encoder::new_to`] to write to an arbitrary [`Write`] sink.
pub struct Encoder<W: Write = Vec<u8>> {
    state: EncodeState<W>,
    registry: SchemaRegistry,
    string_pool: FxHashMap<String, u32>,
    next_pool_id: u32,
    schema_ids: FxHashMap<SchemaKey, WireTypeId>,
}

impl Default for Encoder<Vec<u8>> {
    fn default() -> Self {
        Self::new()
    }
}

impl Encoder<Vec<u8>> {
    pub fn new() -> Self {
        let mut buf = Vec::new();
        codec::encode_header(&mut buf).expect("Vec::write_all cannot fail");
        Self {
            state: EncodeState::new(buf),
            registry: SchemaRegistry::new(),
            string_pool: FxHashMap::default(),
            next_pool_id: 0,
            schema_ids: FxHashMap::default(),
        }
    }

    /// Consume the encoder and return the encoded bytes.
    pub fn finish(self) -> Vec<u8> {
        self.state.writer.into_inner()
    }
}

impl<W: Write> Encoder<W> {
    /// Create an encoder that writes to an arbitrary writer.
    /// Writes the file header immediately.
    pub fn new_to(mut writer: W) -> io::Result<Self> {
        codec::encode_header(&mut writer)?;
        Ok(Self {
            state: EncodeState::new(writer),
            registry: SchemaRegistry::new(),
            string_pool: FxHashMap::default(),
            next_pool_id: 0,
            schema_ids: FxHashMap::default(),
        })
    }

    /// Create an encoder seeded from decoded state. Used by
    /// [`Decoder::into_encoder`](crate::decoder::Decoder::into_encoder).
    pub(crate) fn from_decoder(
        mut registry: SchemaRegistry,
        string_pool: crate::decoder::StringPool,
        timestamp_base_ns: u64,
        writer: W,
    ) -> Self {
        let mut pool = FxHashMap::default();
        let mut next_pool_id: u32 = 0;
        for (id, value) in string_pool.0.into_iter() {
            pool.insert(value, id.raw_id());
            if id.raw_id() >= next_pool_id {
                next_pool_id = id.raw_id() + 1;
            }
        }

        let mut schema_ids = FxHashMap::default();
        for (wire_id, entry) in registry.entries() {
            schema_ids.insert(SchemaKey::Name(Arc::from(entry.name.as_str())), wire_id);
        }
        registry.sync_next_id();

        let mut state = EncodeState::new(writer);
        state.set_ts_base_unchecked(timestamp_base_ns);

        Self {
            state,
            registry,
            string_pool: pool,
            next_pool_id,
            schema_ids,
        }
    }

    /// Consume the encoder and return the inner writer.
    pub fn into_inner(self) -> W {
        self.state.writer.into_inner()
    }

    /// Borrow the inner writer.
    pub fn as_inner(&self) -> &W {
        self.state.writer.inner()
    }

    /// Total bytes written through this encoder (including the file header).
    pub fn bytes_written(&self) -> u64 {
        self.state.writer.bytes_written()
    }

    /// Reset the encoder to a new writer, preserving internal allocations.
    /// Returns the old writer. Writes a file header to the new writer.
    pub fn reset_to(&mut self, mut new_writer: W) -> io::Result<W> {
        codec::encode_header(&mut new_writer)?;
        self.string_pool.clear();
        self.next_pool_id = 0;
        self.registry.clear();
        self.schema_ids.clear();
        // creating a new EncodeState resets the timestamp delta
        let old_state = std::mem::replace(&mut self.state, EncodeState::new(new_writer));
        Ok(old_state.writer.into_inner())
    }

    /// Ensure a schema is registered with this encoder. Returns the wire type
    /// ID for this encoder's output stream.
    ///
    /// Idempotent if the schema matches. Errors if a different schema was
    /// already registered under the same name.
    fn ensure_registered(&mut self, schema: &Schema) -> io::Result<WireTypeId> {
        let key = SchemaKey::Name(Arc::clone(&schema.name_key));
        if let Some(&wire_id) = self.schema_ids.get(&key) {
            // TODO: unify registry and schema_ids to avoid this error case
            let Some(existing) = self.registry.get(wire_id) else {
                return Err(io::Error::other(format!(
                    "corrupted internal state. {wire_id:?} in schema_ids but not in registry."
                )));
            };
            if *existing == *schema.entry {
                return Ok(wire_id);
            }
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                format!(
                    "schema already registered with different definition: {}",
                    schema.name()
                ),
            ));
        }
        let id = self.registry.next_type_id();
        codec::encode_schema(id, &schema.entry, &mut self.state.writer)?;
        self.registry
            .register(id, (*schema.entry).clone())
            .expect("schema registration failed");
        self.schema_ids.insert(key, id);
        Ok(id)
    }

    /// Register a schema by name. Returns a [`Schema`] handle that can be
    /// passed to [`write_event`](Self::write_event) (on this or any other
    /// encoder).
    ///
    /// All schemas have timestamps. When writing events, the first element of
    /// `values` must be `FieldValue::Varint(timestamp_ns)`. It is extracted and
    /// encoded in the event header (not as a regular field).
    ///
    /// Eagerly writes the schema frame. Idempotent if the definition matches.
    pub fn register_schema(
        &mut self,
        name: &str,
        fields: Vec<crate::schema::FieldDef>,
    ) -> io::Result<Schema> {
        let schema = Schema::new(name, fields);
        self.ensure_registered(&schema)?;
        Ok(schema)
    }

    /// Write an event for a schema.
    ///
    /// The first element of `values` must be `FieldValue::Varint(timestamp_ns)`
    /// — it is extracted and encoded in the event header, not as a regular
    /// field. The remaining values must match the schema's field count.
    ///
    /// If this encoder hasn't seen `schema` before, it is auto-registered
    /// (the schema frame is written before the event).
    pub fn write_event(
        &mut self,
        schema: &Schema,
        values: &[crate::types::FieldValue],
    ) -> io::Result<()> {
        use crate::types::FieldValue;

        let type_id = self.ensure_registered(schema)?;
        let expected_fields = schema.entry.fields.len();

        let ts_ns = match values.first() {
            Some(FieldValue::Varint(ns)) => *ns,
            _ => {
                return Err(io::Error::new(
                    io::ErrorKind::InvalidInput,
                    "first value must be FieldValue::Varint(timestamp_ns)",
                ));
            }
        };
        let field_values = &values[1..];

        if field_values.len() != expected_fields {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                format!(
                    "value count ({}) does not match schema field count ({}) for schema '{}'",
                    field_values.len(),
                    expected_fields,
                    schema.name(),
                ),
            ));
        }

        let ts_delta = self.state.encode_timestamp_delta(ts_ns)?;
        self.state.writer.write_all(&[codec::TAG_EVENT])?;
        self.state.writer.write_all(&type_id.0.to_le_bytes())?;
        codec::encode_u24_le(ts_delta, &mut self.state.writer)?;
        let mut enc = EventEncoder::new(&mut self.state);
        for v in field_values {
            enc.write_field_value(v)?;
        }
        Ok(())
    }

    /// Write a derived TraceEvent. Auto-registers the schema on first call for this type.
    /// Handles timestamp encoding: emits TimestampReset if needed, packs u24 delta in header.
    pub fn write<T: TraceEvent + 'static>(&mut self, event: &T) -> io::Result<()> {
        let key = SchemaKey::RustType(TypeId::of::<T>());
        let tid = if let Some(&cached) = self.schema_ids.get(&key) {
            cached
        } else {
            let entry = T::schema_entry();
            let schema = Schema::new(&entry.name, entry.fields);
            let id = self.ensure_registered(&schema)?;
            self.schema_ids.insert(key, id);
            id
        };
        let ts_ns = event.timestamp();
        let ts_delta = self.state.encode_timestamp_delta(ts_ns)?;
        self.state.writer.write_all(&[codec::TAG_EVENT])?;
        self.state.writer.write_all(&tid.0.to_le_bytes())?;
        codec::encode_u24_le(ts_delta, &mut self.state.writer)?;
        let mut enc = EventEncoder::new(&mut self.state);
        event.encode_fields(&mut enc)
    }

    /// Intern a string, emitting a pool frame if new. Returns an [`InternedString`] handle.
    pub fn intern_string(&mut self, s: &str) -> io::Result<InternedString> {
        if let Some(&id) = self.string_pool.get(s) {
            return Ok(InternedString(id));
        }
        let id = self.next_pool_id;
        self.next_pool_id += 1;
        self.string_pool.insert(s.to_string(), id);
        codec::encode_string_pool(
            &[PoolEntry {
                pool_id: id,
                data: s.as_bytes().to_vec(),
            }],
            &mut self.state.writer,
        )?;
        Ok(InternedString(id))
    }

    pub fn write_string_pool(&mut self, entries: &[PoolEntry]) -> io::Result<()> {
        codec::encode_string_pool(entries, &mut self.state.writer)
    }

    /// Flush the underlying writer.
    pub fn flush(&mut self) -> io::Result<()> {
        self.state.writer.flush()
    }

    /// Convert this encoder into a [`RawEncoder`] that only supports writing
    /// pre-encoded bytes. The byte count is preserved so rotation decisions
    /// remain correct.
    ///
    /// Use this after writing any structured data (headers, segment metadata)
    /// to switch to a raw-only mode for appending pre-encoded batches.
    pub fn into_raw_encoder(self) -> RawEncoder<W> {
        RawEncoder {
            writer: self.state.writer,
        }
    }
}

/// A write-only encoder that accepts pre-encoded bytes.
///
/// Created by [`Encoder::into_raw_encoder`] after the file header and any
/// structured metadata have been written. Carries no schema registry, string
/// pool, or timestamp state — it simply forwards bytes to the underlying
/// writer while tracking the total byte count.
pub struct RawEncoder<W> {
    writer: CountingWriter<W>,
}

impl<W: Write> RawEncoder<W> {
    /// Write pre-encoded bytes to the underlying writer.
    pub fn write_raw(&mut self, bytes: &[u8]) -> io::Result<()> {
        self.writer.write_all(bytes)
    }

    /// Total bytes written (including bytes written by the [`Encoder`] before
    /// conversion).
    pub fn bytes_written(&self) -> u64 {
        self.writer.bytes_written()
    }

    /// Flush the underlying writer.
    pub fn flush(&mut self) -> io::Result<()> {
        self.writer.flush()
    }

    /// Consume the raw encoder and return the inner writer.
    pub fn into_inner(self) -> W {
        self.writer.into_inner()
    }
}

impl Encoder<Vec<u8>> {
    pub fn write_infallible<T: TraceEvent + 'static>(&mut self, event: &T) {
        self.write(event).expect("writing to Vec<u8> is infallible")
    }

    pub fn intern_string_infallible(&mut self, s: &str) -> InternedString {
        self.intern_string(s)
            .expect("interning into Vec<u8> is infallible")
    }

    /// Resets the encoder to point to a new backing Vec returning the old one
    pub fn reset_to_infallible(&mut self, data: Vec<u8>) -> Vec<u8> {
        self.reset_to(data)
            .expect("writing to Vec<u8> is infallible")
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::schema::FieldDef;
    use crate::types::{FieldType, FieldValue};

    #[test]
    fn encoder_writes_header() {
        let enc = Encoder::new();
        let data = enc.finish();
        assert_eq!(&data[..5], &[0x54, 0x52, 0x43, 0x00, 1]);
    }

    #[test]
    fn encoder_register_and_write_event() {
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
        assert!(data.len() > 5);
    }

    #[test]
    fn idempotent_re_registration() {
        let mut enc = Encoder::new();
        let fields = vec![FieldDef {
            name: "v".into(),
            field_type: FieldType::Varint,
        }];
        let _s1 = enc.register_schema("Ev", fields.clone()).unwrap();
        let _s2 = enc.register_schema("Ev", fields).unwrap();
        // Both succeed — same schema, same name
    }

    #[test]
    fn re_registration_different_schema_errors() {
        let mut enc = Encoder::new();
        enc.register_schema(
            "Ev",
            vec![FieldDef {
                name: "v".into(),
                field_type: FieldType::Varint,
            }],
        )
        .unwrap();
        let result = enc.register_schema(
            "Ev",
            vec![FieldDef {
                name: "different".into(),
                field_type: FieldType::Bool,
            }],
        );
        assert!(result.is_err());
    }

    #[test]
    fn schema_auto_registers_on_write() {
        use crate::decoder::{DecodedFrame, Decoder};

        // Create a schema without an encoder
        let schema = Schema::new(
            "Lazy",
            vec![FieldDef {
                name: "v".into(),
                field_type: FieldType::Varint,
            }],
        );

        // Write to an encoder that hasn't seen this schema — auto-registers
        let mut enc = Encoder::new();
        enc.write_event(
            &schema,
            &[FieldValue::Varint(1_000), FieldValue::Varint(42)],
        )
        .unwrap();

        let bytes = enc.finish();
        let mut dec = Decoder::new(&bytes).unwrap();
        let frames = dec.decode_all();
        assert!(matches!(&frames[0], DecodedFrame::Schema(s) if s.name == "Lazy"));
        if let DecodedFrame::Event { values, .. } = &frames[1] {
            assert_eq!(*values, vec![FieldValue::Varint(42)]);
        } else {
            panic!("expected event");
        }
    }

    #[test]
    fn schema_portable_across_encoders() {
        use crate::decoder::{DecodedFrame, Decoder};

        let mut enc1 = Encoder::new();
        let schema = enc1
            .register_schema(
                "Shared",
                vec![FieldDef {
                    name: "v".into(),
                    field_type: FieldType::Varint,
                }],
            )
            .unwrap();
        enc1.write_event(&schema, &[FieldValue::Varint(1_000), FieldValue::Varint(1)])
            .unwrap();

        // Pass the same Schema to a different encoder
        let mut enc2 = Encoder::new();
        enc2.write_event(&schema, &[FieldValue::Varint(2_000), FieldValue::Varint(2)])
            .unwrap();

        // Both encoders produce valid output
        for (enc, expected_val) in [(enc1, 1u64), (enc2, 2u64)] {
            let bytes = enc.finish();
            let mut dec = Decoder::new(&bytes).unwrap();
            let frames = dec.decode_all();
            let event = frames
                .iter()
                .find(|f| matches!(f, DecodedFrame::Event { .. }))
                .unwrap();
            if let DecodedFrame::Event { values, .. } = event {
                assert_eq!(values[0], FieldValue::Varint(expected_val));
            }
        }
    }

    #[test]
    fn encoder_intern_string_deduplicates() {
        let mut enc = Encoder::new();
        let id1 = enc.intern_string("hello").unwrap();
        let id2 = enc.intern_string("hello").unwrap();
        let id3 = enc.intern_string("world").unwrap();
        assert_eq!(id1, id2);
        assert_ne!(id1, id3);
    }

    #[test]
    fn timestamp_round_trip() {
        use crate::decoder::{DecodedFrame, Decoder};

        let mut enc = Encoder::new();
        let schema = enc
            .register_schema(
                "TS",
                vec![FieldDef {
                    name: "v".into(),
                    field_type: FieldType::Varint,
                }],
            )
            .unwrap();

        let ts1 = 100_000u64;
        let ts2 = 50_000u64;
        let ts3 = 200_000_000u64;
        let ts4 = 100_000_000u64;
        enc.write_event(&schema, &[FieldValue::Varint(ts1), FieldValue::Varint(1)])
            .unwrap();
        enc.write_event(&schema, &[FieldValue::Varint(ts2), FieldValue::Varint(2)])
            .unwrap();
        enc.write_event(&schema, &[FieldValue::Varint(ts3), FieldValue::Varint(3)])
            .unwrap();
        enc.write_event(&schema, &[FieldValue::Varint(ts4), FieldValue::Varint(4)])
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

        assert_eq!(events.len(), 4);
        assert_eq!(events[0].0, Some(ts1));
        assert_eq!(events[0].1, vec![FieldValue::Varint(1)]);
        assert_eq!(events[1].0, Some(ts2));
        assert_eq!(events[1].1, vec![FieldValue::Varint(2)]);
        assert_eq!(events[2].0, Some(ts3));
        assert_eq!(events[2].1, vec![FieldValue::Varint(3)]);
        assert_eq!(events[3].0, Some(ts4));
        assert_eq!(events[3].1, vec![FieldValue::Varint(4)]);
    }

    #[test]
    fn encoder_new_to_writer() {
        let mut buf = Vec::new();
        let enc = Encoder::new_to(&mut buf).unwrap();
        drop(enc);
        assert!(buf.len() >= 5);
        assert_eq!(&buf[..5], &[0x54, 0x52, 0x43, 0x00, 1]);
    }

    #[test]
    fn decoder_into_encoder_appends_without_header() {
        use crate::decoder::{DecodedFrame, Decoder};

        // Create a trace with a header, a schema, and an event
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
        enc.write_event(&schema, &[FieldValue::Varint(1_000), FieldValue::Varint(1)])
            .unwrap();
        let base = enc.finish();

        // Decode all frames, then convert into an encoder that appends to output
        let mut decoder = Decoder::new(&base).unwrap();
        while decoder.next_frame_ref().ok().flatten().is_some() {}
        let mut output = Vec::new();
        let mut ext = decoder.into_encoder(&mut output);
        // Schema "Ev" is already known — no duplicate schema frame emitted
        ext.write_event(&schema, &[FieldValue::Varint(2_000), FieldValue::Varint(2)])
            .unwrap();
        drop(ext);

        // Concatenate and decode
        let mut combined = base.clone();
        combined.extend_from_slice(&output);
        let mut dec = Decoder::new(&combined).unwrap();
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
        assert_eq!(events[0].0, Some(1_000));
        assert_eq!(events[1].0, Some(2_000));
    }

    #[test]
    fn decoder_into_encoder_deduplicates_interned_strings() {
        use crate::decoder::{DecodedFrame, Decoder};

        // Create a trace with an interned string
        let mut enc = Encoder::new();
        let id1 = enc.intern_string("hello").unwrap();
        let base = enc.finish();

        // Decode all frames, then convert into an encoder
        let mut decoder = Decoder::new(&base).unwrap();
        while decoder.next_frame_ref().ok().flatten().is_some() {}
        let mut output = Vec::new();
        let mut ext = decoder.into_encoder(&mut output);
        // "hello" is already interned, should reuse the same ID
        let id2 = ext.intern_string("hello").unwrap();
        let id3 = ext.intern_string("world").unwrap();
        drop(ext);

        assert_eq!(id1, id2, "existing string should reuse pool ID");
        assert_ne!(id2, id3);

        // "hello" should not produce a new StringPool frame; "world" should
        let mut combined = base.clone();
        combined.extend_from_slice(&output);
        let mut dec = Decoder::new(&combined).unwrap();
        let frames = dec.decode_all();
        let pool_frames: Vec<_> = frames
            .iter()
            .filter(|f| matches!(f, DecodedFrame::StringPool(_)))
            .collect();
        // One from the base trace ("hello"), one from extend ("world")
        assert_eq!(pool_frames.len(), 2);
    }

    #[test]
    fn register_and_write() {
        use crate::decoder::{DecodedFrame, Decoder};

        let mut enc = Encoder::new();
        let schema = enc
            .register_schema(
                "MyEvent",
                vec![
                    FieldDef {
                        name: "count".into(),
                        field_type: FieldType::Varint,
                    },
                    FieldDef {
                        name: "name".into(),
                        field_type: FieldType::String,
                    },
                ],
            )
            .unwrap();

        enc.write_event(
            &schema,
            &[
                FieldValue::Varint(1_000_000),
                FieldValue::Varint(42),
                FieldValue::String("hello".into()),
            ],
        )
        .unwrap();

        let bytes = enc.finish();
        let mut dec = Decoder::new(&bytes).unwrap();
        let frames = dec.decode_all();
        let events: Vec<_> = frames
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
        assert_eq!(events.len(), 1);
        assert_eq!(events[0].0, Some(1_000_000));
        assert_eq!(events[0].1[0], FieldValue::Varint(42));
        assert_eq!(events[0].1[1], FieldValue::String("hello".into()));
    }

    #[test]
    fn register_conflict_errors() {
        let mut enc = Encoder::new();
        enc.register_schema(
            "Ev",
            vec![FieldDef {
                name: "v".into(),
                field_type: FieldType::Varint,
            }],
        )
        .unwrap();
        let result = enc.register_schema(
            "Ev",
            vec![FieldDef {
                name: "other".into(),
                field_type: FieldType::Bool,
            }],
        );
        assert!(result.is_err());
    }

    #[test]
    fn write_wrong_field_count_errors() {
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
        // Pass 3 values (ts + 2 fields) for a 1-field schema
        let result = enc.write_event(
            &schema,
            &[
                FieldValue::Varint(0),
                FieldValue::Varint(1),
                FieldValue::Varint(2),
            ],
        );
        assert!(result.is_err());
    }

    /// Verify that the encoder advances the timestamp base after each event,
    /// producing inter-event deltas rather than base-relative deltas.
    #[test]
    fn timestamp_base_advances_per_event() {
        use crate::decoder::{DecodedFrame, Decoder};

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

        let ts1 = 12_000_000u64;
        let ts2 = 24_000_000u64;
        enc.write_event(&schema, &[FieldValue::Varint(ts1), FieldValue::Varint(1)])
            .unwrap();
        enc.write_event(&schema, &[FieldValue::Varint(ts2), FieldValue::Varint(2)])
            .unwrap();

        let bytes = enc.finish();

        let reset_count = bytes.iter().filter(|&&b| b == 0x05).count();
        assert_eq!(
            reset_count, 0,
            "base should advance per event, avoiding unnecessary resets"
        );

        let mut dec = Decoder::new(&bytes).unwrap();
        let events: Vec<_> = dec
            .decode_all()
            .into_iter()
            .filter_map(|f| match f {
                DecodedFrame::Event { timestamp_ns, .. } => timestamp_ns,
                _ => None,
            })
            .collect();
        assert_eq!(events, vec![ts1, ts2]);
    }

    #[test]
    fn reset_to_preserves_capacity() {
        let mut enc = Encoder::new();
        for i in 0..100 {
            enc.intern_string(&format!("string_{}", i)).unwrap();
        }
        let cap_before = enc.string_pool.capacity();
        let _bytes = enc.reset_to(Vec::new());
        let cap_after = enc.string_pool.capacity();
        assert_eq!(
            cap_before, cap_after,
            "string_pool capacity should be preserved after reset_to"
        );
    }

    #[test]
    fn reset_to_returns_old_data_and_clears_state() {
        use crate::decoder::{DecodedFrame, Decoder};

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
        let _s = enc.intern_string("hello").unwrap();

        let old_bytes_written = enc.bytes_written();
        assert!(old_bytes_written > 0);

        // --- reset ---
        let old = enc.reset_to_infallible(Vec::new());

        // Invariant 1: old writer contains the data we wrote (decodable)
        let mut dec = Decoder::new(&old).unwrap();
        let frames = dec.decode_all();
        assert!(frames.iter().any(|f| matches!(f, DecodedFrame::Schema(_))));
        assert!(
            frames
                .iter()
                .any(|f| matches!(f, DecodedFrame::Event { .. }))
        );
        assert!(
            frames
                .iter()
                .any(|f| matches!(f, DecodedFrame::StringPool(_)))
        );

        // Invariant 2: bytes_written resets to just the header size
        assert!(
            enc.bytes_written() < old_bytes_written,
            "bytes_written should reset (got {} vs old {})",
            enc.bytes_written(),
            old_bytes_written
        );

        // Invariant 3: schemas are cleared — same schema must re-register
        // (write_event auto-registers, so we verify a new schema frame appears)
        enc.write_event(
            &schema,
            &[FieldValue::Varint(2_000), FieldValue::Varint(99)],
        )
        .unwrap();

        // Invariant 4: string pool is cleared — re-interning emits a new pool frame
        let _s2 = enc.intern_string("hello").unwrap();

        // Invariant 5: new output is a valid standalone trace
        let new_bytes = enc.reset_to_infallible(Vec::new());
        let mut dec2 = Decoder::new(&new_bytes).unwrap();
        let new_frames = dec2.decode_all();
        // Must have its own schema definition (not relying on old encoder state)
        assert!(
            new_frames
                .iter()
                .any(|f| matches!(f, DecodedFrame::Schema(s) if s.name == "Ev")),
            "new trace must contain schema definition"
        );
        // Must have its own string pool entry
        assert!(
            new_frames
                .iter()
                .any(|f| matches!(f, DecodedFrame::StringPool(_))),
            "new trace must contain string pool"
        );
        // Event must decode with correct timestamp (timestamp_base was reset)
        let event = new_frames
            .iter()
            .find_map(|f| match f {
                DecodedFrame::Event {
                    timestamp_ns,
                    values,
                    ..
                } => Some((timestamp_ns, values)),
                _ => None,
            })
            .expect("new trace must contain event");
        assert_eq!(*event.0, Some(2_000));
        assert_eq!(event.1[0], FieldValue::Varint(99));
    }

    #[test]
    fn into_raw_encoder_preserves_byte_count() {
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

        let bytes_before = enc.bytes_written();
        assert!(bytes_before > 0);

        let raw = enc.into_raw_encoder();
        assert_eq!(
            raw.bytes_written(),
            bytes_before,
            "byte count must be preserved across conversion"
        );
    }

    #[test]
    fn raw_encoder_write_raw_and_bytes_written() {
        let enc = Encoder::new();
        let initial = enc.bytes_written();
        let mut raw = enc.into_raw_encoder();

        let payload = [0xAA; 100];
        raw.write_raw(&payload).unwrap();

        assert_eq!(
            raw.bytes_written(),
            initial + payload.len() as u64,
            "bytes_written must include raw payload"
        );
    }

    #[test]
    fn raw_encoder_into_inner_returns_all_data() {
        use crate::decoder::{DecodedFrame, Decoder};

        // Write a structured event via Encoder, then append a raw batch
        // via RawEncoder, and verify the combined output decodes correctly.
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
        enc.write_event(&schema, &[FieldValue::Varint(1_000), FieldValue::Varint(1)])
            .unwrap();

        // Build a raw batch with the same schema
        let raw_batch = {
            let mut batch_enc = Encoder::new();
            batch_enc
                .write_event(&schema, &[FieldValue::Varint(2_000), FieldValue::Varint(2)])
                .unwrap();
            batch_enc.finish()
        };

        let mut raw = enc.into_raw_encoder();
        raw.write_raw(&raw_batch).unwrap();
        let combined = raw.into_inner();

        let mut dec = Decoder::new(&combined).unwrap();
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
        assert_eq!(events[0].0, Some(1_000));
        assert_eq!(events[0].1, vec![FieldValue::Varint(1)]);
        assert_eq!(events[1].0, Some(2_000));
        assert_eq!(events[1].1, vec![FieldValue::Varint(2)]);
    }
}
