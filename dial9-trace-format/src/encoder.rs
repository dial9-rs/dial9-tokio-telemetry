// High-level encoder API

use crate::codec::{self, PoolEntry, SymbolEntry, TAG_EVENT};
use crate::schema::{SchemaEntry, SchemaRegistry};
use crate::types::FieldValue;
use crate::TraceEvent;
use std::any::TypeId;
use std::collections::HashMap;

pub struct Encoder {
    buf: Vec<u8>,
    registry: SchemaRegistry,
    string_pool: HashMap<String, u32>,
    next_pool_id: u32,
    // Small linear cache: faster than HashMap+SipHash for the typical <20 event types.
    type_ids: Vec<(TypeId, u16)>,
}

impl Encoder {
    pub fn new() -> Self {
        Self::with_capacity(1024 * 1024)
    }

    pub fn with_capacity(capacity: usize) -> Self {
        let mut buf = Vec::with_capacity(capacity);
        codec::encode_header(&mut buf);
        Self {
            buf, registry: SchemaRegistry::new(),
            string_pool: HashMap::new(), next_pool_id: 0,
            type_ids: Vec::new(),
        }
    }

    fn lookup_or_register<T: TraceEvent + 'static>(&mut self) -> u16 {
        let rust_id = TypeId::of::<T>();
        for &(tid, wire_id) in &self.type_ids {
            if tid == rust_id { return wire_id; }
        }
        let id = self.registry.next_type_id();
        let entry = T::schema_entry(id);
        codec::encode_schema(&entry, &mut self.buf);
        self.registry.register(entry).expect("auto-register failed");
        self.type_ids.push((rust_id, id));
        id
    }

    /// Register a schema for a marker type `T`. The encoder assigns the wire type_id.
    /// Returns the assigned type_id. Panics if `T` is already registered.
    pub fn register_schema_for<T: 'static>(&mut self, name: &str, fields: Vec<crate::schema::FieldDef>) -> u16 {
        let rust_id = TypeId::of::<T>();
        for &(tid, _) in &self.type_ids {
            if tid == rust_id { panic!("type already registered: {name}"); }
        }
        let id = self.registry.next_type_id();
        let entry = SchemaEntry { type_id: id, name: name.to_string(), fields };
        codec::encode_schema(&entry, &mut self.buf);
        self.registry.register(entry).expect("schema registration failed");
        self.type_ids.push((rust_id, id));
        id
    }

    /// Write a derived TraceEvent. Auto-registers the schema on first call for this type.
    pub fn write<T: TraceEvent + 'static>(&mut self, event: &T) {
        let tid = self.lookup_or_register::<T>();
        self.buf.push(TAG_EVENT);
        self.buf.extend_from_slice(&tid.to_le_bytes());
        event.encode_fields(&mut self.buf);
    }

    /// Write an event for a previously registered marker type `T`.
    /// Panics if `T` was not registered via `register_schema_for`.
    pub fn write_event_for<T: 'static>(&mut self, values: &[FieldValue]) {
        let rust_id = TypeId::of::<T>();
        let tid = self.type_ids.iter()
            .find(|(id, _)| *id == rust_id)
            .expect("type not registered; call register_schema_for first")
            .1;
        codec::encode_event(tid, values, &mut self.buf);
    }

    /// Intern a string, emitting a pool frame if new. Returns an [`InternedString`] handle.
    pub fn intern_string(&mut self, s: &str) -> crate::types::InternedString {
        if let Some(&id) = self.string_pool.get(s) {
            return crate::types::InternedString(id);
        }
        let id = self.next_pool_id;
        self.next_pool_id += 1;
        self.string_pool.insert(s.to_string(), id);
        codec::encode_string_pool(&[PoolEntry { pool_id: id, data: s.as_bytes().to_vec() }], &mut self.buf);
        crate::types::InternedString(id)
    }

    pub fn write_string_pool(&mut self, entries: &[PoolEntry]) {
        codec::encode_string_pool(entries, &mut self.buf);
    }

    pub fn write_symbol_table(&mut self, entries: &[SymbolEntry]) {
        codec::encode_symbol_table(entries, &mut self.buf);
    }

    pub fn finish(self) -> Vec<u8> {
        self.buf
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
        enc.register_schema_for::<TestEvent>("Ev", vec![
            FieldDef { name: "v".into(), field_type: FieldType::U64 },
        ]);
        enc.write_event_for::<TestEvent>(&[FieldValue::U64(42)]);
        let data = enc.finish();
        assert!(data.len() > 5);
    }

    struct Unregistered;

    #[test]
    #[should_panic(expected = "type not registered")]
    fn encoder_rejects_unregistered_type() {
        let mut enc = Encoder::new();
        enc.write_event_for::<Unregistered>(&[]);
    }

    #[test]
    fn encoder_intern_string_deduplicates() {
        let mut enc = Encoder::new();
        let id1 = enc.intern_string("hello");
        let id2 = enc.intern_string("hello");
        let id3 = enc.intern_string("world");
        assert_eq!(id1, id2);
        assert_ne!(id1, id3);
    }

    #[test]
    fn encoder_write_symbol_table() {
        let mut enc = Encoder::new();
        enc.write_symbol_table(&[SymbolEntry { base_addr: 0x1000, size: 64, symbol_id: 0 }]);
        let data = enc.finish();
        assert!(data.len() > 5);
    }
}
