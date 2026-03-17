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
///
/// This captures the task id from Tokio as a u64 (sorry)
struct U64Extractor(u64);

impl Hasher for U64Extractor {
    fn write(&mut self, _bytes: &[u8]) {
        debug_assert!(false, "called on bytes!")
    }
    fn write_u64(&mut self, val: u64) {
        self.0 = val;
    }
    fn finish(&self) -> u64 {
        self.0
    }
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

/// Sentinel value for unknown or disabled task tracking.
pub const UNKNOWN_TASK_ID: TaskId = TaskId(0);

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
}
