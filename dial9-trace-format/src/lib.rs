//! # dial9-trace-format
//!
//! A compact binary trace format for recording timestamped events with
//! schema-driven encoding. Events are described by schemas (registered at
//! write time) and encoded with delta-compressed timestamps, LEB128 varints,
//! and an interned string pool.
//!
//! ## Crate layout
//!
//! - [`encoder`] — high-level [`Encoder`](encoder::Encoder) for writing traces
//! - [`decoder`] — streaming [`Decoder`](decoder::Decoder) for reading traces
//! - [`codec`]   — wire-format types ([`WireTypeId`](codec::WireTypeId),
//!   [`PoolEntry`](codec::PoolEntry)) that appear in decoded frames
//! - [`schema`]  — [`SchemaEntry`] and
//!   [`FieldDef`] describing event layouts
//! - [`types`]   — field value types, the [`TraceField`]
//!   trait, and the [`EventEncoder`] used by derived code

pub mod codec;
pub mod decoder;
pub mod encoder;
pub(crate) mod leb128;
pub mod schema;
pub mod types;

pub use dial9_trace_format_derive::TraceEvent;
pub use types::EventEncoder;
pub use types::InternedString;
pub use types::StackFrames;
pub use types::TraceField;

use schema::{FieldDef, SchemaEntry};
use types::FieldValueRef;

/// Trait implemented by `#[derive(TraceEvent)]` for compile-time event types.
pub trait TraceEvent {
    /// Decoded form of this event, potentially borrowing from the input buffer.
    type Ref<'a>;

    /// The event type name (used in schema registration).
    fn event_name() -> &'static str;
    /// Field definitions for schema registration.
    /// When `has_timestamp()` is true, the timestamp is NOT included here —
    /// it is encoded in the event frame header.
    fn field_defs() -> Vec<FieldDef>;
    /// Whether this event type carries a packed timestamp in the event header.
    fn has_timestamp() -> bool {
        true
    }
    /// Return the event's timestamp in nanoseconds.
    fn timestamp(&self) -> u64;
    /// Encode this event's non-timestamp fields into the encoder.
    fn encode_fields<W: std::io::Write>(
        &self,
        enc: &mut types::EventEncoder<'_, W>,
    ) -> std::io::Result<()>;
    /// Decode from a slice of zero-copy field values.
    /// `timestamp_ns` is the absolute timestamp from the event header (if present).
    fn decode<'a>(timestamp_ns: Option<u64>, fields: &[FieldValueRef<'a>])
    -> Option<Self::Ref<'a>>;

    /// Build a SchemaEntry for this event type.
    fn schema_entry() -> SchemaEntry {
        SchemaEntry {
            name: Self::event_name().to_string(),
            has_timestamp: Self::has_timestamp(),
            fields: Self::field_defs(),
        }
    }
}
