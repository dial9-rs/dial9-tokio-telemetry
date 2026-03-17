//! Offline symbolizer: resolves raw stack frame addresses in a trace using
//! captured `/proc/self/maps` data.
//!
//! Reads a trace containing `ProcMapsEntry` events and `StackFrames` fields,
//! resolves addresses via blazesym, and appends `StringPool` + `SymbolTable`
//! frames.

use crate::codec::SymbolEntry;
use crate::decoder::{DecodedFrameRef, Decoder};
use crate::encoder::Encoder;
use crate::schema::FieldDef;
use crate::types::{FieldType, FieldValue, FieldValueRef, InternedString};
use blazesym::symbolize::Symbolizer;
use dial9_perf_self_profile::MapsEntry;
use std::collections::{BTreeSet, HashMap, HashSet};
use std::io::{self, Write};

/// Schema name for proc maps events emitted as regular schema-based events.
pub const PROC_MAPS_EVENT_NAME: &str = "ProcMapsEntry";

/// Marker type for registering the ProcMapsEntry schema via the encoder.
pub struct ProcMapsEventMarker;

/// Register the ProcMapsEntry schema on an encoder and return the assigned type id.
pub fn register_proc_maps_schema<W: Write>(
    encoder: &mut Encoder<W>,
) -> io::Result<crate::codec::WireTypeId> {
    encoder.register_schema_for::<ProcMapsEventMarker>(PROC_MAPS_EVENT_NAME, proc_maps_field_defs())
}

/// Write a single ProcMapsEntry event. The schema must already be registered.
pub fn write_proc_maps_event<W: Write>(
    encoder: &mut Encoder<W>,
    entry: &MapsEntry,
) -> io::Result<()> {
    encoder.write_event_for::<ProcMapsEventMarker>(&[
        FieldValue::Varint(entry.start),
        FieldValue::Varint(entry.end),
        FieldValue::Varint(entry.file_offset),
        FieldValue::String(entry.path.clone()),
    ])
}

fn proc_maps_field_defs() -> Vec<FieldDef> {
    vec![
        FieldDef {
            name: "start".into(),
            field_type: FieldType::Varint,
        },
        FieldDef {
            name: "end".into(),
            field_type: FieldType::Varint,
        },
        FieldDef {
            name: "file_offset".into(),
            field_type: FieldType::Varint,
        },
        FieldDef {
            name: "path".into(),
            field_type: FieldType::String,
        },
    ]
}

/// Symbolize a trace: read the input, resolve addresses from `ProcMapsEntry`
/// events and `StackFrames` data, and write the original trace plus
/// `StringPool` + `SymbolTable` frames to the output.
///
/// The input trace is copied verbatim to the output, with symbol data appended.
pub fn symbolize_trace(input: &[u8], output: &mut impl Write) -> io::Result<()> {
    let mut maps: Vec<MapsEntry> = Vec::new();
    let mut addresses: BTreeSet<u64> = BTreeSet::new();
    let mut proc_maps_type_id: Option<crate::codec::WireTypeId> = None;

    let mut decoder = Decoder::new(input)
        .ok_or_else(|| io::Error::new(io::ErrorKind::InvalidData, "invalid trace header"))?;

    // Single pass: collect ProcMapsEntry events and StackFrames addresses.
    while let Ok(Some(frame)) = decoder.next_frame_ref() {
        match frame {
            DecodedFrameRef::Schema(ref entry) if entry.name == PROC_MAPS_EVENT_NAME => {
                // Find the type_id for this schema by checking what was just registered.
                // The registry assigns sequential IDs, so the latest is the one we want.
                proc_maps_type_id = decoder
                    .registry()
                    .entries()
                    .find(|(_, e)| e.name == PROC_MAPS_EVENT_NAME)
                    .map(|(id, _)| id);
            }
            DecodedFrameRef::Event {
                type_id, values, ..
            } => {
                if Some(type_id) == proc_maps_type_id {
                    if let Some(entry) = try_decode_proc_maps_event(&values) {
                        maps.push(entry);
                    }
                } else {
                    for field in &values {
                        if let FieldValueRef::StackFrames(frames) = field {
                            for addr in frames.iter() {
                                if addr != 0 {
                                    addresses.insert(addr);
                                }
                            }
                        }
                    }
                }
            }
            _ => {}
        }
    }

    if addresses.is_empty() || maps.is_empty() {
        output.write_all(input)?;
        return Ok(());
    }

    // Resolve symbols.
    let symbolizer = Symbolizer::new();
    let mut pool_entries: Vec<crate::codec::PoolEntry> = Vec::new();
    let mut symbol_entries: Vec<SymbolEntry> = Vec::new();
    let mut string_to_id: HashMap<String, u32> = HashMap::new();
    let mut seen_base_addrs: HashSet<u64> = HashSet::new();
    let mut next_pool_id: u32 = 0;

    for &addr in &addresses {
        let symbols = dial9_perf_self_profile::resolve_symbols_with_maps(addr, &symbolizer, &maps);
        for info in symbols {
            let Some(name) = info.name else { continue };
            if !seen_base_addrs.insert(info.base_addr) {
                continue;
            }

            let pool_id = *string_to_id.entry(name.clone()).or_insert_with(|| {
                let id = next_pool_id;
                next_pool_id += 1;
                pool_entries.push(crate::codec::PoolEntry {
                    pool_id: id,
                    data: name.into_bytes(),
                });
                id
            });

            symbol_entries.push(SymbolEntry {
                base_addr: info.base_addr,
                size: 0, // unknown; blazesym doesn't provide function size
                symbol_id: InternedString(pool_id),
            });
        }
    }

    // Write: original trace verbatim + string pool + symbol table.
    output.write_all(input)?;
    if !pool_entries.is_empty() {
        crate::codec::encode_string_pool(&pool_entries, output)?;
    }
    if !symbol_entries.is_empty() {
        crate::codec::encode_symbol_table(&symbol_entries, output)?;
    }

    Ok(())
}

/// Try to decode a ProcMapsEntry from event field values.
/// Expected shape: [Varint(start), Varint(end), Varint(file_offset), String(path)]
fn try_decode_proc_maps_event(values: &[FieldValueRef<'_>]) -> Option<MapsEntry> {
    if values.len() != 4 {
        return None;
    }
    let start = match &values[0] {
        FieldValueRef::Varint(v) => *v,
        _ => return None,
    };
    let end = match &values[1] {
        FieldValueRef::Varint(v) => *v,
        _ => return None,
    };
    let file_offset = match &values[2] {
        FieldValueRef::Varint(v) => *v,
        _ => return None,
    };
    let path = match &values[3] {
        FieldValueRef::String(s) => (*s).to_string(),
        _ => return None,
    };
    Some(MapsEntry {
        start,
        end,
        file_offset,
        path,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::decoder::Decoder;
    use crate::encoder::Encoder;

    #[test]
    fn symbolize_empty_trace() {
        let enc = Encoder::new();
        let input = enc.finish();
        let mut output = Vec::new();
        symbolize_trace(&input, &mut output).unwrap();
        assert_eq!(input, output);
    }

    #[test]
    fn symbolize_no_proc_maps_copies_verbatim() {
        struct Ev;
        let mut enc = Encoder::new();
        enc.register_schema_for::<Ev>(
            "Ev",
            vec![FieldDef {
                name: "frames".into(),
                field_type: FieldType::StackFrames,
            }],
        )
        .unwrap();
        enc.write_event_for::<Ev>(&[FieldValue::StackFrames(vec![0x1000])])
            .unwrap();
        let input = enc.finish();
        let mut output = Vec::new();
        symbolize_trace(&input, &mut output).unwrap();
        assert_eq!(input, output);
    }

    #[test]
    fn symbolize_no_stack_frames_copies_verbatim() {
        let mut enc = Encoder::new();
        register_proc_maps_schema(&mut enc).unwrap();
        write_proc_maps_event(
            &mut enc,
            &MapsEntry {
                start: 0x1000,
                end: 0x2000,
                file_offset: 0,
                path: "/bin/test".into(),
            },
        )
        .unwrap();
        let input = enc.finish();
        let mut output = Vec::new();
        symbolize_trace(&input, &mut output).unwrap();
        assert_eq!(input, output);
    }

    #[test]
    fn symbolize_unresolvable_addresses_produces_valid_trace() {
        struct Ev;
        let mut enc = Encoder::new();
        enc.register_schema_for::<Ev>(
            "Ev",
            vec![FieldDef {
                name: "frames".into(),
                field_type: FieldType::StackFrames,
            }],
        )
        .unwrap();
        enc.write_event_for::<Ev>(&[FieldValue::StackFrames(vec![0x55a4b2c01000])])
            .unwrap();
        register_proc_maps_schema(&mut enc).unwrap();
        write_proc_maps_event(
            &mut enc,
            &MapsEntry {
                start: 0x55a4b2c00000,
                end: 0x55a4b2c05000,
                file_offset: 0x1000,
                path: "/nonexistent/binary".into(),
            },
        )
        .unwrap();
        let input = enc.finish();

        let mut output = Vec::new();
        symbolize_trace(&input, &mut output).unwrap();

        // Output must be a valid trace regardless of whether symbols resolved.
        assert!(output.len() >= input.len());
        let mut dec = Decoder::new(&output).unwrap();
        let frames = dec.decode_all();
        // schema(Ev) + event + schema(ProcMapsEntry) + event = 4 minimum
        assert!(frames.len() >= 4);
    }

    /// Symbolize a real address from the current binary to verify the
    /// end-to-end flow produces StringPool + SymbolTable frames.
    #[cfg(target_os = "linux")]
    #[test]
    fn symbolize_real_address_produces_symbol_table() {
        // Use the address of this test function itself.
        let addr = symbolize_real_address_produces_symbol_table as *const () as u64;
        let raw_maps = dial9_perf_self_profile::read_proc_maps();
        assert!(
            raw_maps.iter().any(|m| addr >= m.start && addr < m.end),
            "test function address {addr:#x} not found in any mapping"
        );

        struct Ev;
        let mut enc = Encoder::new();
        enc.register_schema_for::<Ev>(
            "Ev",
            vec![FieldDef {
                name: "frames".into(),
                field_type: FieldType::StackFrames,
            }],
        )
        .unwrap();
        enc.write_event_for::<Ev>(&[FieldValue::StackFrames(vec![addr])])
            .unwrap();
        register_proc_maps_schema(&mut enc).unwrap();
        for m in &raw_maps {
            write_proc_maps_event(&mut enc, m).unwrap();
        }
        let input = enc.finish();

        let mut output = Vec::new();
        symbolize_trace(&input, &mut output).unwrap();

        // Output should be larger (symbol data appended).
        assert!(
            output.len() > input.len(),
            "expected symbol data to be appended"
        );

        // Decode and verify we got StringPool + SymbolTable frames.
        let mut dec = Decoder::new(&output).unwrap();
        let frames = dec.decode_all();
        let has_string_pool = frames
            .iter()
            .any(|f| matches!(f, crate::decoder::DecodedFrame::StringPool(_)));
        let has_symbol_table = frames
            .iter()
            .any(|f| matches!(f, crate::decoder::DecodedFrame::SymbolTable(_)));
        assert!(has_string_pool, "expected StringPool frame in output");
        assert!(has_symbol_table, "expected SymbolTable frame in output");
    }

    #[test]
    fn proc_maps_event_round_trip() {
        let mut enc = Encoder::new();
        register_proc_maps_schema(&mut enc).unwrap();
        let entry = MapsEntry {
            start: 0x55a4b2c00000,
            end: 0x55a4b2c05000,
            file_offset: 0x1000,
            path: "/usr/bin/foo".into(),
        };
        write_proc_maps_event(&mut enc, &entry).unwrap();
        let data = enc.finish();

        let mut dec = Decoder::new(&data).unwrap();
        let frames = dec.decode_all_ref();
        // Schema + Event
        assert_eq!(frames.len(), 2);
        if let DecodedFrameRef::Event { values, .. } = &frames[1] {
            let decoded = try_decode_proc_maps_event(values).unwrap();
            assert_eq!(decoded.start, entry.start);
            assert_eq!(decoded.end, entry.end);
            assert_eq!(decoded.file_offset, entry.file_offset);
            assert_eq!(decoded.path, entry.path);
        } else {
            panic!("expected event frame");
        }
    }
}
