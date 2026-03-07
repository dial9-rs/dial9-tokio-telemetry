// Schema types and registry

use crate::types::FieldType;
use std::collections::HashMap;

#[derive(Debug, Clone, PartialEq)]
pub struct FieldDef {
    pub name: String,
    pub field_type: FieldType,
}

#[derive(Debug, Clone, PartialEq)]
pub struct SchemaEntry {
    pub type_id: u16,
    pub name: String,
    pub fields: Vec<FieldDef>,
}

#[derive(Debug, Default)]
pub struct SchemaRegistry {
    schemas: HashMap<u16, SchemaEntry>,
}

impl SchemaRegistry {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn register(&mut self, entry: SchemaEntry) -> Result<(), String> {
        if let Some(existing) = self.schemas.get(&entry.type_id) {
            if *existing == entry {
                return Ok(());
            }
            return Err(format!("type_id {} already registered with different schema", entry.type_id));
        }
        self.schemas.insert(entry.type_id, entry);
        Ok(())
    }

    pub fn get(&self, type_id: u16) -> Option<&SchemaEntry> {
        self.schemas.get(&type_id)
    }

    pub fn entries(&self) -> impl Iterator<Item = &SchemaEntry> {
        self.schemas.values()
    }

    pub fn next_type_id(&self) -> u16 {
        self.schemas.keys().max().map_or(0, |m| m + 1)
    }
}

/// Builder for runtime schema registration.
pub struct SchemaBuilder {
    name: String,
    fields: Vec<FieldDef>,
}

impl SchemaBuilder {
    pub fn new(name: &str) -> Self {
        Self { name: name.to_string(), fields: Vec::new() }
    }

    pub fn field(mut self, name: &str, field_type: FieldType) -> Self {
        self.fields.push(FieldDef { name: name.to_string(), field_type });
        self
    }

    /// Register with the given registry, auto-assigning a type_id.
    pub fn register(self, registry: &mut SchemaRegistry) -> Result<u16, String> {
        let type_id = registry.next_type_id();
        let entry = SchemaEntry { type_id, name: self.name, fields: self.fields };
        registry.register(entry)?;
        Ok(type_id)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn register_and_lookup() {
        let mut reg = SchemaRegistry::new();
        let entry = SchemaEntry {
            type_id: 1,
            name: "PollStart".into(),
            fields: vec![
                FieldDef { name: "timestamp_ns".into(), field_type: FieldType::U64 },
                FieldDef { name: "worker".into(), field_type: FieldType::U64 },
            ],
        };
        reg.register(entry.clone()).unwrap();
        assert_eq!(reg.get(1), Some(&entry));
        assert_eq!(reg.get(99), None);
    }

    #[test]
    fn duplicate_type_id_same_schema_ok() {
        let mut reg = SchemaRegistry::new();
        let entry = SchemaEntry { type_id: 1, name: "A".into(), fields: vec![] };
        reg.register(entry.clone()).unwrap();
        reg.register(entry).unwrap();
    }

    #[test]
    fn duplicate_type_id_different_schema_rejected() {
        let mut reg = SchemaRegistry::new();
        reg.register(SchemaEntry { type_id: 1, name: "A".into(), fields: vec![] }).unwrap();
        assert!(reg.register(SchemaEntry { type_id: 1, name: "B".into(), fields: vec![] }).is_err());
    }

    #[test]
    fn multiple_schemas() {
        let mut reg = SchemaRegistry::new();
        reg.register(SchemaEntry { type_id: 1, name: "A".into(), fields: vec![] }).unwrap();
        reg.register(SchemaEntry { type_id: 2, name: "B".into(), fields: vec![] }).unwrap();
        assert_eq!(reg.entries().count(), 2);
    }

    #[test]
    fn schema_builder_registers() {
        let mut reg = SchemaRegistry::new();
        let id = SchemaBuilder::new("http_request")
            .field("timestamp_ns", FieldType::U64)
            .field("method", FieldType::String)
            .field("status", FieldType::U64)
            .register(&mut reg)
            .unwrap();
        let entry = reg.get(id).unwrap();
        assert_eq!(entry.name, "http_request");
        assert_eq!(entry.fields.len(), 3);
        assert_eq!(entry.fields[0].name, "timestamp_ns");
        assert_eq!(entry.fields[1].field_type, FieldType::String);
    }

    #[test]
    fn schema_builder_auto_increments_id() {
        let mut reg = SchemaRegistry::new();
        let id1 = SchemaBuilder::new("A").register(&mut reg).unwrap();
        let id2 = SchemaBuilder::new("B").register(&mut reg).unwrap();
        assert_ne!(id1, id2);
    }
}
