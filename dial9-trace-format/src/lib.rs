pub mod codec;
pub mod decoder;
pub mod encoder;
pub mod leb128;
pub mod schema;
pub mod types;

pub use dial9_trace_format_derive::TraceEvent;
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
    fn field_defs() -> Vec<FieldDef>;
    /// Encode this event's fields directly into a buffer.
    fn encode_fields(&self, buf: &mut Vec<u8>);
    /// Decode from a slice of zero-copy field values.
    fn decode<'a>(fields: &[FieldValueRef<'a>]) -> Option<Self::Ref<'a>>;
    /// Build a SchemaEntry for this event type with the given type_id.
    fn schema_entry(type_id: u16) -> SchemaEntry {
        SchemaEntry {
            type_id,
            name: Self::event_name().to_string(),
            fields: Self::field_defs(),
        }
    }
}
