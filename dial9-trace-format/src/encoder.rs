// High-level encoder API

use crate::TraceEvent;
use crate::codec::{self, PoolEntry, SymbolEntry, WireTypeId};
use crate::schema::{SchemaEntry, SchemaRegistry};
use crate::types::{EncodeState, EventEncoder};
use std::any::TypeId;
use std::collections::HashMap;
use std::io::{self, Write};

/// Key for looking up a previously registered schema.
#[derive(Clone, PartialEq, Eq, Hash)]
enum SchemaKey {
    /// Keyed by Rust `TypeId` (compile-time types via `register_schema_for`).
    Static(TypeId),
    /// Keyed by name string (runtime-defined types via `register_dynamic_schema`).
    Dynamic(String),
}

pub struct Encoder<W: Write = Vec<u8>> {
    state: EncodeState<W>,
    registry: SchemaRegistry,
    string_pool: HashMap<String, u32>,
    next_pool_id: u32,
    schema_ids: HashMap<SchemaKey, WireTypeId>,
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
            string_pool: HashMap::new(),
            next_pool_id: 0,
            schema_ids: HashMap::new(),
        }
    }

    /// Consume the encoder and return the encoded bytes.
    pub fn finish(self) -> Vec<u8> {
        self.state.writer
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
            string_pool: HashMap::new(),
            next_pool_id: 0,
            schema_ids: HashMap::new(),
        })
    }

    /// Consume the encoder and return the inner writer.
    pub fn into_inner(self) -> W {
        self.state.writer
    }

    /// Register a schema under a given key. Idempotent if the schema matches;
    /// errors if the same key was already registered with a different schema.
    fn register_schema_impl(
        &mut self,
        key: SchemaKey,
        entry: SchemaEntry,
    ) -> io::Result<WireTypeId> {
        if let Some(&wire_id) = self.schema_ids.get(&key) {
            let existing = self.registry.get(wire_id).unwrap();
            if *existing == entry {
                return Ok(wire_id);
            }
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                format!(
                    "schema already registered with different definition: {}",
                    entry.name
                ),
            ));
        }
        let id = self.registry.next_type_id();
        codec::encode_schema(id, &entry, &mut self.state.writer)?;
        self.registry
            .register(id, entry)
            .expect("schema registration failed");
        self.schema_ids.insert(key, id);
        Ok(id)
    }

    /// Write field values for an event with the given wire type ID.
    fn write_event_impl(
        &mut self,
        type_id: WireTypeId,
        values: &[crate::types::FieldValue],
    ) -> io::Result<()> {
        use crate::types::FieldValue;

        let schema = self.registry.get(type_id).ok_or_else(|| {
            io::Error::new(
                io::ErrorKind::InvalidInput,
                format!("unknown type_id: {type_id:?}"),
            )
        })?;
        let has_timestamp = schema.has_timestamp;
        let expected_fields = schema.fields.len();

        let (ts_delta, field_values) = if has_timestamp {
            let ts_ns = match values.first() {
                Some(FieldValue::Varint(ns)) => *ns,
                _ => {
                    panic!(
                        "has_timestamp schema requires first value to be FieldValue::Varint(timestamp_ns)"
                    )
                }
            };
            let delta = self.state.encode_timestamp_delta(ts_ns)?;
            (Some(delta), &values[1..])
        } else {
            (None, values)
        };

        assert_eq!(
            field_values.len(),
            expected_fields,
            "value count does not match schema field count for type_id {type_id:?}"
        );

        self.state.writer.write_all(&[codec::TAG_EVENT])?;
        self.state.writer.write_all(&type_id.0.to_le_bytes())?;
        if let Some(delta) = ts_delta {
            codec::encode_u24_le(delta, &mut self.state.writer)?;
        }
        let mut enc = EventEncoder::new(&mut self.state);
        for v in field_values {
            enc.write_field_value(v)?;
        }
        Ok(())
    }

    fn lookup_or_register<T: TraceEvent + 'static>(&mut self) -> io::Result<WireTypeId> {
        let key = SchemaKey::Static(TypeId::of::<T>());
        self.register_schema_impl(key, T::schema_entry())
    }

    /// Register a schema for a marker type `T`. The encoder assigns the wire type_id.
    /// Returns the assigned type_id, or the existing type_id if `T` was already registered
    /// with the same schema (idempotent). Returns an error if `T` was already registered
    /// with a different schema.
    pub fn register_schema_for<T: 'static>(
        &mut self,
        name: &str,
        fields: Vec<crate::schema::FieldDef>,
    ) -> io::Result<WireTypeId> {
        self.register_schema_for_with_timestamp::<T>(name, false, fields)
    }

    /// Register a schema with explicit timestamp flag.
    /// Returns the assigned type_id, or the existing type_id if `T` was already registered
    /// with the same schema (idempotent). Returns an error if `T` was already registered
    /// with a different schema.
    pub fn register_schema_for_with_timestamp<T: 'static>(
        &mut self,
        name: &str,
        has_timestamp: bool,
        fields: Vec<crate::schema::FieldDef>,
    ) -> io::Result<WireTypeId> {
        let key = SchemaKey::Static(TypeId::of::<T>());
        let entry = SchemaEntry {
            name: name.to_string(),
            has_timestamp,
            fields,
        };
        self.register_schema_impl(key, entry)
    }

    /// Write a derived TraceEvent. Auto-registers the schema on first call for this type.
    /// Handles timestamp encoding: emits TimestampReset if needed, packs u24 delta in header.
    pub fn write<T: TraceEvent + 'static>(&mut self, event: &T) -> io::Result<()> {
        let tid = self.lookup_or_register::<T>()?;
        let ts_delta = if let Some(ts_ns) = event.timestamp() {
            Some(self.state.encode_timestamp_delta(ts_ns)?)
        } else {
            None
        };
        self.state.writer.write_all(&[codec::TAG_EVENT])?;
        self.state.writer.write_all(&tid.0.to_le_bytes())?;
        if let Some(delta) = ts_delta {
            codec::encode_u24_le(delta, &mut self.state.writer)?;
        }
        let mut enc = EventEncoder::new(&mut self.state);
        event.encode_fields(&mut enc)
    }

    /// Write an event for a previously registered marker type `T`.
    /// For schemas with `has_timestamp`, pass the timestamp as the first element
    /// of `values` as `FieldValue::Varint(ns)` — it will be extracted and
    /// encoded in the header.
    pub fn write_event_for<T: 'static>(
        &mut self,
        values: &[crate::types::FieldValue],
    ) -> io::Result<()> {
        let key = SchemaKey::Static(TypeId::of::<T>());
        let tid = *self
            .schema_ids
            .get(&key)
            .expect("type not registered; call register_schema_for first");
        self.write_event_impl(tid, values)
    }

    /// Register a schema by name without requiring a compile-time Rust type.
    /// Returns the assigned [`WireTypeId`].
    ///
    /// All dynamic schemas have timestamps. The timestamp is passed as the first
    /// element when writing events via [`write_dynamic_event`](Self::write_dynamic_event).
    ///
    /// Idempotent: re-registering the same name with the same fields returns the
    /// existing ID. Returns an error if the name was already registered with a
    /// different schema.
    pub fn register_dynamic_schema(
        &mut self,
        name: &str,
        fields: Vec<crate::schema::FieldDef>,
    ) -> io::Result<WireTypeId> {
        let key = SchemaKey::Dynamic(name.to_string());
        let entry = SchemaEntry {
            name: name.to_string(),
            has_timestamp: true,
            fields,
        };
        self.register_schema_impl(key, entry)
    }

    /// Write an event for a dynamically registered schema (see
    /// [`register_dynamic_schema`](Self::register_dynamic_schema)).
    ///
    /// `timestamp_ns` is encoded in the event header. `values` contains the
    /// field values (must match the schema's field count).
    pub fn write_dynamic_event(
        &mut self,
        type_id: WireTypeId,
        timestamp_ns: u64,
        values: &[crate::types::FieldValue],
    ) -> io::Result<()> {
        let expected_fields = self
            .registry
            .get(type_id)
            .ok_or_else(|| {
                io::Error::new(
                    io::ErrorKind::InvalidInput,
                    format!("unknown dynamic type_id: {type_id:?}"),
                )
            })?
            .fields
            .len();

        if values.len() != expected_fields {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                format!(
                    "value count ({}) does not match schema field count ({}) for type_id {type_id:?}",
                    values.len(),
                    expected_fields,
                ),
            ));
        }

        let ts_delta = self.state.encode_timestamp_delta(timestamp_ns)?;
        self.state.writer.write_all(&[codec::TAG_EVENT])?;
        self.state.writer.write_all(&type_id.0.to_le_bytes())?;
        codec::encode_u24_le(ts_delta, &mut self.state.writer)?;
        let mut enc = EventEncoder::new(&mut self.state);
        for v in values {
            enc.write_field_value(v)?;
        }
        Ok(())
    }

    /// Intern a string, emitting a pool frame if new. Returns an [`InternedString`] handle.
    pub fn intern_string(&mut self, s: &str) -> io::Result<crate::types::InternedString> {
        if let Some(&id) = self.string_pool.get(s) {
            return Ok(crate::types::InternedString(id));
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
        Ok(crate::types::InternedString(id))
    }

    pub fn write_string_pool(&mut self, entries: &[PoolEntry]) -> io::Result<()> {
        codec::encode_string_pool(entries, &mut self.state.writer)
    }

    pub fn write_symbol_table(&mut self, entries: &[SymbolEntry]) -> io::Result<()> {
        codec::encode_symbol_table(entries, &mut self.state.writer)
    }

    /// Flush the underlying writer.
    pub fn flush(&mut self) -> io::Result<()> {
        self.state.writer.flush()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::schema::FieldDef;
    use crate::types::FieldType;

    #[test]
    fn encoder_writes_header() {
        let enc = Encoder::new();
        let data = enc.finish();
        assert_eq!(&data[..5], &[0x54, 0x52, 0x43, 0x00, 1]);
    }

    struct TestEvent;

    #[test]
    fn encoder_register_and_write_event() {
        let mut enc = Encoder::new();
        enc.register_schema_for::<TestEvent>(
            "Ev",
            vec![FieldDef {
                name: "v".into(),
                field_type: FieldType::Varint,
            }],
        )
        .unwrap();
        enc.write_event_for::<TestEvent>(&[crate::types::FieldValue::Varint(42)])
            .unwrap();
        let data = enc.finish();
        assert!(data.len() > 5);
    }

    struct Unregistered;

    #[test]
    #[should_panic(expected = "type not registered")]
    fn encoder_rejects_unregistered_type() {
        let mut enc = Encoder::new();
        enc.write_event_for::<Unregistered>(&[]).unwrap();
    }

    #[test]
    fn idempotent_re_registration() {
        let mut enc = Encoder::new();
        let fields = vec![FieldDef {
            name: "v".into(),
            field_type: FieldType::Varint,
        }];
        let id1 = enc
            .register_schema_for::<TestEvent>("Ev", fields.clone())
            .unwrap();
        let id2 = enc.register_schema_for::<TestEvent>("Ev", fields).unwrap();
        assert_eq!(id1, id2);
    }

    #[test]
    fn re_registration_different_schema_errors() {
        let mut enc = Encoder::new();
        enc.register_schema_for::<TestEvent>(
            "Ev",
            vec![FieldDef {
                name: "v".into(),
                field_type: FieldType::Varint,
            }],
        )
        .unwrap();
        let result = enc.register_schema_for::<TestEvent>(
            "Ev",
            vec![FieldDef {
                name: "different".into(),
                field_type: FieldType::Bool,
            }],
        );
        assert!(result.is_err());
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
    fn encoder_write_symbol_table() {
        let mut enc = Encoder::new();
        enc.write_symbol_table(&[SymbolEntry {
            base_addr: 0x1000,
            size: 64,
            symbol_id: 0,
        }])
        .unwrap();
        let data = enc.finish();
        assert!(data.len() > 5);
    }

    #[test]
    fn timestamp_round_trip_write_event_for() {
        use crate::decoder::{DecodedFrame, Decoder};
        use crate::types::FieldValue;

        struct TS;
        let mut enc = Encoder::new();
        enc.register_schema_for_with_timestamp::<TS>(
            "TS",
            true,
            vec![FieldDef {
                name: "v".into(),
                field_type: FieldType::Varint,
            }],
        )
        .unwrap();

        let ts1 = 100_000u64;
        let ts2 = 50_000u64; // backwards — but base is still 0, so no reset needed
        let ts3 = 200_000_000u64; // big jump — triggers reset
        enc.write_event_for::<TS>(&[FieldValue::Varint(ts1), FieldValue::Varint(1)])
            .unwrap();
        enc.write_event_for::<TS>(&[FieldValue::Varint(ts2), FieldValue::Varint(2)])
            .unwrap();
        enc.write_event_for::<TS>(&[FieldValue::Varint(ts3), FieldValue::Varint(3)])
            .unwrap();
        // Now go backwards from ts3's reset base
        let ts4 = 100_000_000u64;
        enc.write_event_for::<TS>(&[FieldValue::Varint(ts4), FieldValue::Varint(4)])
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
        assert_eq!(
            events[3].0,
            Some(ts4),
            "backwards from reset base must be preserved"
        );
        assert_eq!(events[3].1, vec![FieldValue::Varint(4)]);
    }

    #[test]
    fn encoder_new_to_writer() {
        let mut buf = Vec::new();
        let enc = Encoder::new_to(&mut buf).unwrap();
        drop(enc);
        // Should at least have the header
        assert!(buf.len() >= 5);
        assert_eq!(&buf[..5], &[0x54, 0x52, 0x43, 0x00, 1]);
    }

    #[test]
    fn dynamic_register_and_write() {
        use crate::decoder::{DecodedFrame, Decoder};
        use crate::types::FieldValue;

        let mut enc = Encoder::new();
        let tid = enc
            .register_dynamic_schema(
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

        enc.write_dynamic_event(
            tid,
            1_000_000,
            &[FieldValue::Varint(42), FieldValue::String("hello".into())],
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
    fn dynamic_register_idempotent() {
        let mut enc = Encoder::new();
        let fields = vec![FieldDef {
            name: "v".into(),
            field_type: FieldType::Varint,
        }];
        let id1 = enc
            .register_dynamic_schema("Ev", fields.clone())
            .unwrap();
        let id2 = enc.register_dynamic_schema("Ev", fields).unwrap();
        assert_eq!(id1, id2);
    }

    #[test]
    fn dynamic_register_conflict_errors() {
        let mut enc = Encoder::new();
        enc.register_dynamic_schema(
            "Ev",
            vec![FieldDef {
                name: "v".into(),
                field_type: FieldType::Varint,
            }],
        )
        .unwrap();
        let result = enc.register_dynamic_schema(
            "Ev",
            vec![FieldDef {
                name: "other".into(),
                field_type: FieldType::Bool,
            }],
        );
        assert!(result.is_err());
    }

    #[test]
    fn dynamic_write_wrong_field_count_errors() {
        let mut enc = Encoder::new();
        let tid = enc
            .register_dynamic_schema(
                "Ev",
                vec![FieldDef {
                    name: "v".into(),
                    field_type: FieldType::Varint,
                }],
            )
            .unwrap();
        // Pass 2 values for a 1-field schema
        let result = enc.write_dynamic_event(
            tid,
            0,
            &[
                crate::types::FieldValue::Varint(1),
                crate::types::FieldValue::Varint(2),
            ],
        );
        assert!(result.is_err());
    }

    /// Verify that the encoder advances the timestamp base after each event,
    /// producing inter-event deltas rather than base-relative deltas.
    /// This is critical for compression: without it, backwards timestamps
    /// trigger reset frames that destroy gzip patterns.
    #[test]
    fn timestamp_base_advances_per_event() {
        use crate::decoder::{DecodedFrame, Decoder};
        use crate::types::FieldValue;

        struct Ev;
        let mut enc = Encoder::new();
        enc.register_schema_for_with_timestamp::<Ev>(
            "Ev",
            true,
            vec![FieldDef {
                name: "v".into(),
                field_type: FieldType::Varint,
            }],
        )
        .unwrap();

        // Two events 12ms apart. Both fit in u24 from base=0.
        // But if the base advances after event 1, event 2's delta is only 12ms.
        // If the base doesn't advance, event 2's delta is 24ms > 16.7ms → unnecessary reset.
        let ts1 = 12_000_000u64; // 12ms
        let ts2 = 24_000_000u64; // 24ms (12ms after ts1)
        enc.write_event_for::<Ev>(&[FieldValue::Varint(ts1), FieldValue::Varint(1)])
            .unwrap();
        enc.write_event_for::<Ev>(&[FieldValue::Varint(ts2), FieldValue::Varint(2)])
            .unwrap();

        let bytes = enc.finish();

        // Count TimestampReset frames (tag 0x05) in the raw bytes.
        // With base-advancing: 0 resets (both deltas fit in u24).
        // Without base-advancing: 1 reset (ts2's delta from base=0 is 24ms > 16.7ms).
        let reset_count = bytes.iter().filter(|&&b| b == 0x05).count();
        assert_eq!(
            reset_count, 0,
            "base should advance per event, avoiding unnecessary resets"
        );

        // Also verify timestamps decode correctly
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
}
