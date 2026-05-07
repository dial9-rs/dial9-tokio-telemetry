//! Schema types describing event layouts.
//!
//! A [`SchemaEntry`] defines the name and fields of an event type. The
//! [`SchemaRegistry`] tracks all registered schemas and assigns wire type IDs.

use crate::codec::WireTypeId;
use crate::encoder::FxHashMap;
use crate::types::FieldType;
use std::borrow::Cow;

/// A per-field annotation carrying arbitrary key-value metadata.
///
/// Annotations are emitted in a separate frame (`TAG_SCHEMA_ANNOTATIONS`)
/// after the schema frame they belong to. They carry metadata such as units,
/// display hints, or semantic-convention labels.
#[non_exhaustive]
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FieldAnnotation {
    field_index: u16,
    key: Cow<'static, str>,
    value: Cow<'static, str>,
}

impl FieldAnnotation {
    /// Create a new field annotation.
    pub fn new(field_index: u16, key: impl Into<String>, value: impl Into<String>) -> Self {
        Self {
            field_index,
            key: Cow::Owned(key.into()),
            value: Cow::Owned(value.into()),
        }
    }

    /// Index of the field this annotation applies to (0-based, matching the
    /// field order in [`SchemaEntry::fields`]).
    pub fn field_index(&self) -> u16 {
        self.field_index
    }

    /// Annotation key (e.g. `"metrique.unit"`).
    pub fn key(&self) -> &str {
        &self.key
    }

    /// Annotation value (e.g. `"microseconds"`).
    pub fn value(&self) -> &str {
        &self.value
    }
}

/// A single field within a schema: a name and a [`FieldType`].
#[non_exhaustive]
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FieldDef {
    /// Field name (e.g. `"worker_id"`).
    pub name: String,
    /// Wire type used to encode this field. Optional variants (e.g.
    /// `FieldType::OptionalPooledString`) indicate the field uses the
    /// high-bit optional encoding on the wire.
    pub field_type: FieldType,
}

impl FieldDef {
    /// Construct a field definition with the given name and type.
    ///
    /// ```
    /// # use dial9_trace_format::schema::FieldDef;
    /// # use dial9_trace_format::types::FieldType;
    /// FieldDef::new("worker_id", FieldType::Varint);
    /// FieldDef::new("tags", FieldType::DynamicList);
    /// ```
    pub fn new(name: impl Into<String>, field_type: FieldType) -> Self {
        Self {
            name: name.into(),
            field_type,
        }
    }
}

/// Describes the layout of an event type. Does not carry a wire type ID —
/// the ID is assigned by the encoder and tracked externally by the registry.
#[non_exhaustive]
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SchemaEntry {
    /// Event type name (e.g. `"PollStart"`).
    pub name: String,
    /// Whether events of this type carry a packed u24 nanosecond timestamp in the event header.
    pub has_timestamp: bool,
    /// Ordered list of fields (excluding the timestamp, which is in the header).
    pub fields: Vec<FieldDef>,
    /// Per-field annotations (metadata such as units, display hints).
    /// Empty by default; carried in a separate wire frame.
    pub annotations: Vec<FieldAnnotation>,
}

impl SchemaEntry {
    /// Construct a new schema entry.
    pub fn new(name: impl Into<String>, has_timestamp: bool, fields: Vec<FieldDef>) -> Self {
        Self {
            name: name.into(),
            has_timestamp,
            fields,
            annotations: Vec::new(),
        }
    }

    /// Construct a schema entry with annotations.
    pub fn with_annotations(
        name: impl Into<String>,
        has_timestamp: bool,
        fields: Vec<FieldDef>,
        annotations: Vec<FieldAnnotation>,
    ) -> Self {
        Self {
            name: name.into(),
            has_timestamp,
            fields,
            annotations,
        }
    }
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
            annotations: Vec::new(),
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
            annotations: Vec::new(),
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
                annotations: Vec::new(),
            },
        )
        .unwrap();
        assert!(
            reg.register(
                id,
                SchemaEntry {
                    name: "B".into(),
                    has_timestamp: true,
                    fields: vec![],
                    annotations: Vec::new(),
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
                annotations: Vec::new(),
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
                annotations: Vec::new(),
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
