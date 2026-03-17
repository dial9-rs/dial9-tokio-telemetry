//! Task metadata types for telemetry tracking.

use serde::Serialize;
use std::hash::{Hash, Hasher};

/// Task identifier. Stores a u64 hash of tokio's task ID for compact wire format.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Default)]
pub struct TaskId(pub(crate) u64);

impl TaskId {
    pub fn from_u32(id: u32) -> Self {
        TaskId(id as u64)
    }

    pub fn to_u64(self) -> u64 {
        self.0
    }

    pub fn id(&self) -> u64 {
        self.0
    }
}

impl std::fmt::Display for TaskId {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "TaskId({})", self.0)
    }
}

impl Serialize for TaskId {
    fn serialize<S: serde::Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        serializer.serialize_u64(self.0)
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

/// Sentinel value for unknown or disabled task tracking.
pub const UNKNOWN_TASK_ID: TaskId = TaskId(0);
