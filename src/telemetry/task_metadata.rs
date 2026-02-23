//! Task metadata types for telemetry tracking.

use serde::Serialize;
use std::hash::{Hash, Hasher};

/// Task identifier. Stores a u64 hash of tokio's task ID for compact wire format.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Default)]
pub struct TaskId(u64);

impl Serialize for TaskId {
    fn serialize<S: serde::Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        serializer.serialize_u32(self.to_u32())
    }
}

impl From<tokio::task::Id> for TaskId {
    fn from(id: tokio::task::Id) -> Self {
        // Extract the raw u64 from tokio's opaque Id using a capturing hasher
        let mut extractor = U64Extractor(0);
        id.hash(&mut extractor);
        TaskId(extractor.0)
    }
}

/// A Hasher that captures the first u64 written to it.
struct U64Extractor(u64);

impl Hasher for U64Extractor {
    fn write(&mut self, _bytes: &[u8]) {}
    fn write_u64(&mut self, val: u64) { self.0 = val; }
    fn finish(&self) -> u64 { self.0 }
}

impl TaskId {
    /// Convert to u32 for wire format (truncate upper bits).
    pub const fn to_u32(self) -> u32 {
        self.0 as u32
    }

    /// Create from u32 (for testing or deserialization).
    pub const fn from_u32(val: u32) -> Self {
        TaskId(val as u64)
    }
}

/// Spawn location identifier. Maps to a spawn location string via lookup table.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Default)]
pub struct SpawnLocationId(pub u16);

impl Serialize for SpawnLocationId {
    fn serialize<S: serde::Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        serializer.serialize_u16(self.as_u16())
    }
}

impl SpawnLocationId {
    pub const fn as_u16(self) -> u16 {
        self.0
    }

    pub const fn from_u16(val: u16) -> Self {
        SpawnLocationId(val)
    }
}

/// Sentinel value for unknown or disabled task tracking.
pub const UNKNOWN_TASK_ID: TaskId = TaskId(0);

/// Sentinel value for unknown or disabled spawn location tracking.
pub const UNKNOWN_SPAWN_LOCATION_ID: SpawnLocationId = SpawnLocationId(0);

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_task_id_default() {
        let id = TaskId::default();
        assert_eq!(id, UNKNOWN_TASK_ID);
        assert_eq!(id.to_u32(), 0);
    }

    #[test]
    fn test_spawn_location_id_default() {
        let id = SpawnLocationId::default();
        assert_eq!(id.as_u16(), 0);
        assert_eq!(id, UNKNOWN_SPAWN_LOCATION_ID);
    }

    #[test]
    fn test_task_id_roundtrip() {
        let task_id = TaskId::from_u32(12345);
        assert_eq!(task_id.to_u32(), 12345);
    }

    #[test]
    fn test_task_id_hash() {
        use std::collections::HashSet;
        let mut set = HashSet::new();
        let id1 = TaskId::from_u32(1);
        let id2 = TaskId::from_u32(2);
        set.insert(id1);
        set.insert(id2);
        assert!(set.contains(&id1));
        assert!(!set.contains(&TaskId::from_u32(3)));
    }

    #[test]
    fn test_spawn_location_id_hash() {
        use std::collections::HashSet;
        let mut set = HashSet::new();
        set.insert(SpawnLocationId(1));
        set.insert(SpawnLocationId(2));
        assert!(set.contains(&SpawnLocationId(1)));
        assert!(!set.contains(&SpawnLocationId(3)));
    }

    #[test]
    fn test_spawn_location_id_roundtrip() {
        let id = SpawnLocationId::from_u16(42);
        assert_eq!(id.as_u16(), 42);
    }
}
