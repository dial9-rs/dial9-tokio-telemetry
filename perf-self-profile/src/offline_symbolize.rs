//! Offline symbolizer: resolves raw stack frame addresses in a trace using
//! captured `/proc/self/maps` data.
//!
//! Reads a trace containing `ProcMapsEntry` events and `StackFrames` fields,
//! resolves addresses via blazesym, and appends `SymbolTableEntry` events
//! (with a `StringPool` frame for symbol names).

use dial9_trace_format::{
    decoder::{Decoder, StackPool},
    types::{FieldValueRef, InternedString},
};
use std::collections::HashSet;
use std::io::{self, Write};

use crate::MapsEntry;

/// Schema-based event for resolved symbol table entries.
///
/// Each entry maps an instruction pointer address to a resolved symbol name.
/// When a function has inlined callees, multiple entries share the same `addr`
/// with increasing `inline_depth` (0 = outermost).
#[derive(dial9_trace_format::TraceEvent)]
pub struct SymbolTableEntry {
    #[traceevent(timestamp)]
    pub timestamp_ns: u64,
    pub addr: u64,
    pub size: u64,
    pub symbol_name: InternedString,
    /// 0 = outermost function, 1+ = inlined callee depth.
    pub inline_depth: u64,
    /// Source file path from debug info (e.g. `/home/user/.cargo/registry/src/.../hyper-0.14.28/src/client.rs`).
    // TODO: consider splitting out source_file and source_dir to allow avoiding an extra allocation during interning.
    pub source_file: InternedString,
    /// Source line number, or 0 if unavailable.
    pub source_line: u64,
}

/// Symbolize a trace using caller-provided proc maps instead of reading them
/// from the trace.
///
/// Use this when the caller already has the memory mappings (e.g. from
/// `read_proc_maps()` in the same process). This avoids the overhead of
/// encoding proc maps into the trace and re-parsing them.
///
/// On non-Linux platforms this is a no-op (returns `Ok(())`).
pub fn symbolize_trace_with_maps(
    input: &[u8],
    maps: &[MapsEntry],
    output: &mut impl Write,
) -> io::Result<()> {
    let mut addresses: HashSet<u64> = HashSet::new();

    let mut decoder = Decoder::new(input)
        .ok_or_else(|| io::Error::new(io::ErrorKind::InvalidData, "invalid trace header"))?;

    decoder
        .for_each_event(|event| {
            collect_stack_frame_addresses(event.fields, event.stack_pool, &mut addresses);
        })
        .map_err(|e| io::Error::new(io::ErrorKind::InvalidData, e.to_string()))?;

    if addresses.is_empty() {
        return Ok(());
    }

    crate::sys::write_symbol_data(decoder, &addresses, maps, output)
}

fn collect_stack_frame_addresses(
    values: &[FieldValueRef<'_>],
    stack_pool: &StackPool,
    addresses: &mut HashSet<u64>,
) {
    for field in values {
        match field {
            FieldValueRef::StackFrames(frames) => {
                for addr in frames.iter() {
                    if addr != 0 {
                        addresses.insert(addr);
                    }
                }
            }
            FieldValueRef::PooledStackFrames(id) => {
                if let Some(frames) = stack_pool.get(*id) {
                    for &addr in frames {
                        if addr != 0 {
                            addresses.insert(addr);
                        }
                    }
                }
            }
            _ => {}
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use dial9_trace_format::{
        decoder::{DecodedFrame, Decoder},
        encoder::Encoder,
        schema::FieldDef,
        types::{FieldType, FieldValue},
    };

    #[test]
    fn symbol_table_event_round_trip() {
        let mut enc = Encoder::new();
        let sym_name = enc.intern_string("my_function").unwrap();
        let src_file = enc.intern_string("/src/lib.rs").unwrap();
        enc.write(&SymbolTableEntry {
            timestamp_ns: 0,
            addr: 0x1000,
            size: 256,
            symbol_name: sym_name,
            inline_depth: 0,
            source_file: src_file,
            source_line: 42,
        })
        .unwrap();
        let buf = enc.finish();

        let mut dec = Decoder::new(&buf).unwrap();
        let frames = dec.decode_all();
        // StringPool("my_function") + StringPool("/src/lib.rs") + Schema + Event
        assert_eq!(frames.len(), 4);
        if let DecodedFrame::Event { values, .. } = &frames[3] {
            assert_eq!(values[0], FieldValue::Varint(0x1000));
            assert_eq!(values[1], FieldValue::Varint(256));
            assert_eq!(
                values[2],
                FieldValue::PooledString(InternedString::from_raw(0))
            );
            assert_eq!(values[3], FieldValue::Varint(0));
            assert_eq!(
                values[4],
                FieldValue::PooledString(InternedString::from_raw(1))
            );
            assert_eq!(values[5], FieldValue::Varint(42));
        } else {
            panic!("expected event frame");
        }
        assert_eq!(
            dec.string_pool().get(InternedString::from_raw(0)),
            Some("my_function")
        );
    }

    #[test]
    fn symbolize_empty_trace_writes_nothing() {
        let buf = Encoder::new().finish();
        let mut output = Vec::new();
        symbolize_trace_with_maps(&buf, &[], &mut output).unwrap();
        assert!(output.is_empty());
    }

    #[test]
    fn symbolize_no_stack_frames_writes_nothing() {
        let mut enc = Encoder::new();
        let schema = enc
            .register_schema("Ev", vec![FieldDef::new("count", FieldType::Varint)])
            .unwrap();
        enc.write_event(&schema, &[FieldValue::Varint(0), FieldValue::Varint(42)])
            .unwrap();
        let buf = enc.finish();

        let maps = vec![MapsEntry {
            start: 0x1000,
            end: 0x2000,
            file_offset: 0,
            path: "/bin/test".into(),
        }];
        let mut output = Vec::new();
        symbolize_trace_with_maps(&buf, &maps, &mut output).unwrap();
        assert!(output.is_empty());
    }

    #[test]
    fn symbolize_empty_maps_writes_nothing() {
        let mut enc = Encoder::new();
        let schema = enc
            .register_schema("Ev", vec![FieldDef::new("frames", FieldType::StackFrames)])
            .unwrap();
        enc.write_event(
            &schema,
            &[
                FieldValue::Varint(0),
                FieldValue::StackFrames(vec![0x1000].into()),
            ],
        )
        .unwrap();
        let buf = enc.finish();

        let mut output = Vec::new();
        symbolize_trace_with_maps(&buf, &[], &mut output).unwrap();
        // Addresses exist but none match any mapping, so no symbols are emitted.
        assert!(output.is_empty());
    }
}
