//! Offline symbolizer: resolves raw stack frame addresses in a trace using
//! captured `/proc/self/maps` data.
//!
//! Reads a trace containing `ProcMapsEntry` events and `StackFrames` fields,
//! resolves addresses via blazesym, and appends `SymbolTableEntry` events
//! (with a `StringPool` frame for symbol names).

use blazesym::symbolize::Symbolizer;
use dial9_trace_format::{
    TraceEvent,
    codec::{self, WireTypeId},
    decoder::{DecodedFrameRef, Decoder},
    types::{FieldValue, FieldValueRef, InternedString},
};
use std::collections::{BTreeSet, HashMap};
use std::io::{self, Write};

use crate::MapsEntry;

/// Schema-based event for capturing `/proc/self/maps` entries in a trace.
#[derive(dial9_trace_format::TraceEvent)]
pub struct ProcMapsEntry {
    #[traceevent(timestamp)]
    pub timestamp_ns: u64,
    pub start: u64,
    pub end: u64,
    pub file_offset: u64,
    pub path: String,
}

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
    pub source_file: InternedString,
    /// Source line number, or 0 if unavailable.
    pub source_line: u64,
}

/// Write ProcMapsEntry events to a writer using low-level codec functions.
///
/// Writes the schema frame followed by one event per entry.
pub fn encode_proc_maps(
    type_id: WireTypeId,
    entries: &[MapsEntry],
    w: &mut impl Write,
) -> io::Result<()> {
    codec::encode_schema(type_id, &ProcMapsEntry::schema_entry(), w)?;
    for e in entries {
        codec::encode_event(
            type_id,
            Some(0),
            &[
                FieldValue::Varint(e.start),
                FieldValue::Varint(e.end),
                FieldValue::Varint(e.file_offset),
                FieldValue::String(e.path.clone()),
            ],
            w,
        )?;
    }
    Ok(())
}

/// Symbolize a trace: read `input` for `ProcMapsEntry` events and `StackFrames`
/// fields, resolve addresses via blazesym, and write only the new
/// `SymbolTableEntry` frames (with a `StringPool`) to `output`.
///
/// The caller is responsible for the original trace data. In the typical
/// file-based workflow, open the trace file in append mode and pass it as
/// `output` so symbols are appended in place without copying.
pub fn symbolize_trace(input: &[u8], output: &mut impl Write) -> io::Result<()> {
    let proc_maps_name = ProcMapsEntry::event_name();

    let mut maps: Vec<MapsEntry> = Vec::new();
    let mut addresses: BTreeSet<u64> = BTreeSet::new();
    let mut proc_maps_type_id: Option<WireTypeId> = None;

    let mut decoder = Decoder::new(input)
        .ok_or_else(|| io::Error::new(io::ErrorKind::InvalidData, "invalid trace header"))?;

    while let Ok(Some(frame)) = decoder.next_frame_ref() {
        match frame {
            DecodedFrameRef::Schema(ref entry) if entry.name == proc_maps_name => {
                proc_maps_type_id = decoder
                    .registry()
                    .entries()
                    .find(|(_, e)| e.name == proc_maps_name)
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
                    collect_stack_frame_addresses(&values, &mut addresses);
                }
            }
            _ => {}
        }
    }

    if addresses.is_empty() || maps.is_empty() {
        return Ok(());
    }

    write_symbol_data(&addresses, &maps, output)
}

/// Symbolize a trace using caller-provided proc maps instead of reading them
/// from the trace.
///
/// Use this when the caller already has the memory mappings (e.g. from
/// `read_proc_maps()` in the same process). This avoids the overhead of
/// encoding proc maps into the trace and re-parsing them.
pub fn symbolize_trace_with_maps(
    input: &[u8],
    maps: &[MapsEntry],
    output: &mut impl Write,
) -> io::Result<()> {
    let mut addresses: BTreeSet<u64> = BTreeSet::new();

    let mut decoder = Decoder::new(input)
        .ok_or_else(|| io::Error::new(io::ErrorKind::InvalidData, "invalid trace header"))?;

    while let Ok(Some(frame)) = decoder.next_frame_ref() {
        if let DecodedFrameRef::Event { values, .. } = frame {
            collect_stack_frame_addresses(&values, &mut addresses);
        }
    }

    if addresses.is_empty() {
        return Ok(());
    }

    write_symbol_data(&addresses, maps, output)
}

fn collect_stack_frame_addresses(values: &[FieldValueRef<'_>], addresses: &mut BTreeSet<u64>) {
    for field in values {
        if let FieldValueRef::StackFrames(frames) = field {
            for addr in frames.iter() {
                if addr != 0 {
                    addresses.insert(addr);
                }
            }
        }
    }
}

struct ResolvedSymbol {
    addr: u64,
    symbol_name: InternedString,
    inline_depth: u64,
    source_file: InternedString,
    source_line: u64,
}

fn write_symbol_data(
    addresses: &BTreeSet<u64>,
    maps: &[MapsEntry],
    output: &mut impl Write,
) -> io::Result<()> {
    let symbolizer = Symbolizer::new();
    let mut pool_entries: Vec<codec::PoolEntry> = Vec::new();
    let mut string_to_id: HashMap<String, u32> = HashMap::new();
    let mut next_pool_id: u32 = 0;
    let mut symbol_events: Vec<ResolvedSymbol> = Vec::new();

    let intern = |s: String,
                  pool_entries: &mut Vec<codec::PoolEntry>,
                  next_pool_id: &mut u32,
                  string_to_id: &mut HashMap<String, u32>|
     -> InternedString {
        let id = *string_to_id.entry(s.clone()).or_insert_with(|| {
            let id = *next_pool_id;
            *next_pool_id += 1;
            pool_entries.push(codec::PoolEntry {
                pool_id: id,
                data: s.into_bytes(),
            });
            id
        });
        InternedString::from_raw(id)
    };

    for &addr in addresses {
        let symbols = crate::resolve_symbols_with_maps(addr, &symbolizer, maps);
        for (depth, info) in symbols.into_iter().enumerate() {
            let Some(name) = info.name else { continue };

            let symbol_name = intern(
                name,
                &mut pool_entries,
                &mut next_pool_id,
                &mut string_to_id,
            );

            let (source_file_str, source_line) = match info.code_info {
                Some(ci) => (ci.file, ci.line.unwrap_or(0) as u64),
                None => (String::new(), 0),
            };

            let source_file = intern(
                source_file_str,
                &mut pool_entries,
                &mut next_pool_id,
                &mut string_to_id,
            );

            symbol_events.push(ResolvedSymbol {
                addr,
                symbol_name,
                inline_depth: depth as u64,
                source_file,
                source_line,
            });
        }
    }

    if !symbol_events.is_empty() {
        codec::encode_string_pool(&pool_entries, output)?;

        let type_id = WireTypeId(0xFFFE);
        codec::encode_schema(type_id, &SymbolTableEntry::schema_entry(), output)?;
        for sym in &symbol_events {
            codec::encode_event(
                type_id,
                Some(0),
                &[
                    FieldValue::Varint(sym.addr),
                    FieldValue::Varint(0),
                    FieldValue::PooledString(sym.symbol_name),
                    FieldValue::Varint(sym.inline_depth),
                    FieldValue::PooledString(sym.source_file),
                    FieldValue::Varint(sym.source_line),
                ],
                output,
            )?;
        }
    }

    Ok(())
}

fn try_decode_proc_maps_event(values: &[FieldValueRef<'_>]) -> Option<MapsEntry> {
    let decoded = ProcMapsEntry::decode(Some(0), values)?;
    Some(MapsEntry {
        start: decoded.start,
        end: decoded.end,
        file_offset: decoded.file_offset,
        path: decoded.path.to_string(),
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use dial9_trace_format::{
        decoder::{DecodedFrame, Decoder},
        encoder::Encoder,
        schema::{FieldDef, SchemaEntry},
        types::FieldType,
    };

    fn make_encoder_with_proc_maps(maps: &[MapsEntry]) -> Vec<u8> {
        let mut buf = Encoder::new().finish();
        encode_proc_maps(WireTypeId(100), maps, &mut buf).unwrap();
        buf
    }

    #[test]
    fn symbolize_empty_trace() {
        let input = Encoder::new().finish();
        let mut output = Vec::new();
        symbolize_trace(&input, &mut output).unwrap();
        assert!(output.is_empty());
    }

    #[test]
    fn symbolize_no_proc_maps_writes_nothing() {
        let mut buf = Encoder::new().finish();
        let schema = SchemaEntry {
            name: "Ev".into(),
            has_timestamp: false,
            fields: vec![FieldDef {
                name: "frames".into(),
                field_type: FieldType::StackFrames,
            }],
        };
        let tid = WireTypeId(0);
        codec::encode_schema(tid, &schema, &mut buf).unwrap();
        codec::encode_event(
            tid,
            None,
            &[FieldValue::StackFrames(vec![0x1000])],
            &mut buf,
        )
        .unwrap();

        let mut output = Vec::new();
        symbolize_trace(&buf, &mut output).unwrap();
        assert!(output.is_empty());
    }

    #[test]
    fn symbolize_no_stack_frames_writes_nothing() {
        let input = make_encoder_with_proc_maps(&[MapsEntry {
            start: 0x1000,
            end: 0x2000,
            file_offset: 0,
            path: "/bin/test".into(),
        }]);
        let mut output = Vec::new();
        symbolize_trace(&input, &mut output).unwrap();
        assert!(output.is_empty());
    }

    #[test]
    fn symbolize_unresolvable_addresses_produces_valid_trace() {
        let mut buf = Encoder::new().finish();
        let ev_schema = SchemaEntry {
            name: "Ev".into(),
            has_timestamp: false,
            fields: vec![FieldDef {
                name: "frames".into(),
                field_type: FieldType::StackFrames,
            }],
        };
        let tid = WireTypeId(0);
        codec::encode_schema(tid, &ev_schema, &mut buf).unwrap();
        codec::encode_event(
            tid,
            None,
            &[FieldValue::StackFrames(vec![0x55a4b2c01000])],
            &mut buf,
        )
        .unwrap();
        encode_proc_maps(
            WireTypeId(1),
            &[MapsEntry {
                start: 0x55a4b2c00000,
                end: 0x55a4b2c05000,
                file_offset: 0x1000,
                path: "/nonexistent/binary".into(),
            }],
            &mut buf,
        )
        .unwrap();

        let mut output = Vec::new();
        symbolize_trace(&buf, &mut output).unwrap();
        // Unresolvable addresses produce no symbol events
        let mut combined = buf.clone();
        combined.extend_from_slice(&output);
        let mut dec = Decoder::new(&combined).unwrap();
        let frames = dec.decode_all();
        assert!(frames.len() >= 4);
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn symbolize_real_address_produces_symbol_events() {
        let addr = symbolize_real_address_produces_symbol_events as *const () as u64;
        let raw_maps = crate::read_proc_maps();
        assert!(
            raw_maps.iter().any(|m| addr >= m.start && addr < m.end),
            "test function address {addr:#x} not found in any mapping"
        );

        let mut buf = Encoder::new().finish();
        let ev_schema = SchemaEntry {
            name: "Ev".into(),
            has_timestamp: false,
            fields: vec![FieldDef {
                name: "frames".into(),
                field_type: FieldType::StackFrames,
            }],
        };
        let tid = WireTypeId(0);
        codec::encode_schema(tid, &ev_schema, &mut buf).unwrap();
        codec::encode_event(tid, None, &[FieldValue::StackFrames(vec![addr])], &mut buf).unwrap();
        encode_proc_maps(WireTypeId(1), &raw_maps, &mut buf).unwrap();

        let mut output = Vec::new();
        symbolize_trace(&buf, &mut output).unwrap();

        assert!(!output.is_empty(), "expected symbol data to be written");

        let symbol_table_name = SymbolTableEntry::event_name();
        let mut combined = buf.clone();
        combined.extend_from_slice(&output);
        let mut dec = Decoder::new(&combined).unwrap();
        let frames = dec.decode_all();
        let has_string_pool = frames
            .iter()
            .any(|f| matches!(f, DecodedFrame::StringPool(_)));
        let has_symbol_schema = frames
            .iter()
            .any(|f| matches!(f, DecodedFrame::Schema(s) if s.name == symbol_table_name));
        assert!(has_string_pool, "expected StringPool frame in output");
        assert!(
            has_symbol_schema,
            "expected SymbolTableEntry schema in output"
        );
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn symbolize_with_maps_produces_symbol_events() {
        let addr = symbolize_with_maps_produces_symbol_events as *const () as u64;
        let raw_maps = crate::read_proc_maps();

        let mut buf = Encoder::new().finish();
        let tid = WireTypeId(0);
        codec::encode_schema(
            tid,
            &SchemaEntry {
                name: "Ev".into(),
                has_timestamp: false,
                fields: vec![FieldDef {
                    name: "frames".into(),
                    field_type: FieldType::StackFrames,
                }],
            },
            &mut buf,
        )
        .unwrap();
        codec::encode_event(tid, None, &[FieldValue::StackFrames(vec![addr])], &mut buf).unwrap();

        let mut output = Vec::new();
        symbolize_trace_with_maps(&buf, &raw_maps, &mut output).unwrap();

        assert!(!output.is_empty(), "expected symbol data to be written");

        let mut combined = buf.clone();
        combined.extend_from_slice(&output);
        let mut dec = Decoder::new(&combined).unwrap();
        let frames = dec.decode_all();
        let has_string_pool = frames
            .iter()
            .any(|f| matches!(f, DecodedFrame::StringPool(_)));
        let has_symbol_schema = frames.iter().any(
            |f| matches!(f, DecodedFrame::Schema(s) if s.name == SymbolTableEntry::event_name()),
        );
        assert!(has_string_pool, "expected StringPool frame in output");
        assert!(
            has_symbol_schema,
            "expected SymbolTableEntry schema in output"
        );
    }

    #[test]
    fn proc_maps_event_round_trip() {
        let entry = MapsEntry {
            start: 0x55a4b2c00000,
            end: 0x55a4b2c05000,
            file_offset: 0x1000,
            path: "/usr/bin/foo".into(),
        };
        let mut buf = Vec::new();
        codec::encode_header(&mut buf).unwrap();
        encode_proc_maps(WireTypeId(0), &[entry.clone()], &mut buf).unwrap();

        let mut dec = Decoder::new(&buf).unwrap();
        let frames = dec.decode_all_ref();
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

    #[test]
    fn symbol_table_event_round_trip() {
        let mut buf = Vec::new();
        codec::encode_header(&mut buf).unwrap();
        codec::encode_string_pool(
            &[
                codec::PoolEntry {
                    pool_id: 0,
                    data: b"my_function".to_vec(),
                },
                codec::PoolEntry {
                    pool_id: 1,
                    data: b"/src/lib.rs".to_vec(),
                },
            ],
            &mut buf,
        )
        .unwrap();
        let tid = WireTypeId(0);
        codec::encode_schema(tid, &SymbolTableEntry::schema_entry(), &mut buf).unwrap();
        codec::encode_event(
            tid,
            Some(0),
            &[
                FieldValue::Varint(0x1000),
                FieldValue::Varint(256),
                FieldValue::PooledString(InternedString::from_raw(0)),
                FieldValue::Varint(0),
                FieldValue::PooledString(InternedString::from_raw(1)),
                FieldValue::Varint(42),
            ],
            &mut buf,
        )
        .unwrap();

        let mut dec = Decoder::new(&buf).unwrap();
        let frames = dec.decode_all();
        // StringPool + Schema + Event
        assert_eq!(frames.len(), 3);
        if let DecodedFrame::Event { values, .. } = &frames[2] {
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
}
