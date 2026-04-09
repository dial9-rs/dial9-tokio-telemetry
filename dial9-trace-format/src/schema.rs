//! Schema types describing event layouts.
//!
//! A [`SchemaEntry`] defines the name and fields of an event type. The
//! [`SchemaRegistry`] tracks all registered schemas and assigns wire type IDs.

use crate::codec::WireTypeId;
use crate::encoder::FxHashMap;
use crate::types::FieldType;

/// A single field within a schema: a name and a [`FieldType`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FieldDef {
    /// Field name (e.g. `"worker_id"`).
    pub name: String,
    /// Wire type used to encode this field.
    pub field_type: FieldType,
}

/// Describes the layout of an event type. Does not carry a wire type ID —
/// the ID is assigned by the encoder and tracked externally by the registry.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SchemaEntry {
    /// Event type name (e.g. `"PollStart"`).
    pub name: String,
    /// Whether events of this type carry a packed u24 nanosecond timestamp in the event header.
    pub has_timestamp: bool,
    /// Ordered list of fields (excluding the timestamp, which is in the header).
    pub fields: Vec<FieldDef>,
}

#[derive(Debug, Default, Clone)]
pub struct SchemaRegistry {
    pub(crate) schemas: FxHashMap<WireTypeId, SchemaEntry>,
    pub(crate) next_id: u16,
}

impl SchemaRegistry {
    pub fn new() -> Self {
        Self::default()
    }

    /// Resets the schema registry to a blank slate without releasing the allocations
    pub fn clear(&mut self) {
        self.next_id = 0;
        self.schemas.clear();
    }

    /// Register a schema under the given wire type ID.
    pub fn register(&mut self, type_id: WireTypeId, entry: SchemaEntry) -> Result<(), String> {
        if let Some(existing) = self.schemas.get(&type_id) {
            if *existing == entry {
                return Ok(());
            }
            return Err(format!(
                "type_id {:?} already registered with different schema",
                type_id
            ));
        }
        self.schemas.insert(type_id, entry);
        Ok(())
    }

    pub fn get(&self, type_id: WireTypeId) -> Option<&SchemaEntry> {
        self.schemas.get(&type_id)
    }

    pub fn entries(&self) -> impl Iterator<Item = (WireTypeId, &SchemaEntry)> {
        self.schemas.iter().map(|(&id, entry)| (id, entry))
    }

    /// Allocate the next wire type ID.
    pub fn next_type_id(&mut self) -> WireTypeId {
        let id = WireTypeId(self.next_id);
        self.next_id += 1;
        id
    }

    /// Advance `next_id` past all registered type IDs.
    ///
    /// Call this after bulk-inserting schemas (e.g. from a decoded trace) so
    /// that [`next_type_id`](Self::next_type_id) won't collide.
    pub fn sync_next_id(&mut self) {
        for &id in self.schemas.keys() {
            if id.0 >= self.next_id {
                self.next_id = id.0 + 1;
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn register_and_lookup() {
        let mut reg = SchemaRegistry::new();
        let id = reg.next_type_id();
        let entry = SchemaEntry {
            name: "PollStart".into(),
            has_timestamp: true,
            fields: vec![
                FieldDef {
                    name: "timestamp_ns".into(),
                    field_type: FieldType::Varint,
                },
                FieldDef {
                    name: "worker".into(),
                    field_type: FieldType::Varint,
                },
            ],
        };
        reg.register(id, entry.clone()).unwrap();
        assert_eq!(reg.get(id), Some(&entry));
        assert_eq!(reg.get(WireTypeId(99)), None);
    }

    #[test]
    fn duplicate_type_id_same_schema_ok() {
        let mut reg = SchemaRegistry::new();
        let id = reg.next_type_id();
        let entry = SchemaEntry {
            name: "A".into(),
            has_timestamp: true,
            fields: vec![],
        };
        reg.register(id, entry.clone()).unwrap();
        reg.register(id, entry).unwrap();
    }

    #[test]
    fn duplicate_type_id_different_schema_rejected() {
        let mut reg = SchemaRegistry::new();
        let id = reg.next_type_id();
        reg.register(
            id,
            SchemaEntry {
                name: "A".into(),
                has_timestamp: true,
                fields: vec![],
            },
        )
        .unwrap();
        assert!(
            reg.register(
                id,
                SchemaEntry {
                    name: "B".into(),
                    has_timestamp: true,
                    fields: vec![]
                }
            )
            .is_err()
        );
    }

    #[test]
    fn multiple_schemas() {
        let mut reg = SchemaRegistry::new();
        let id1 = reg.next_type_id();
        reg.register(
            id1,
            SchemaEntry {
                name: "A".into(),
                has_timestamp: true,
                fields: vec![],
            },
        )
        .unwrap();
        let id2 = reg.next_type_id();
        reg.register(
            id2,
            SchemaEntry {
                name: "B".into(),
                has_timestamp: true,
                fields: vec![],
            },
        )
        .unwrap();
        assert_eq!(reg.entries().count(), 2);
    }

    #[test]
    fn next_type_id_auto_increments() {
        let mut reg = SchemaRegistry::new();
        let id1 = reg.next_type_id();
        let id2 = reg.next_type_id();
        assert_ne!(id1, id2);
        assert_eq!(id1, WireTypeId(0));
        assert_eq!(id2, WireTypeId(1));
    }
}
