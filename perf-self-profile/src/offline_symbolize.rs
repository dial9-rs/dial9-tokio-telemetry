//! Offline symbolizer: resolves raw stack frame addresses in a trace using
//! captured `/proc/self/maps` data.
//!
//! Reads a trace containing `ProcMapsEntry` events and `StackFrames` fields,
//! resolves addresses via blazesym, and appends `SymbolTableEntry` events
//! (with a `StringPool` frame for symbol names).

use blazesym::symbolize::Symbolizer;
use dial9_trace_format::{
    codec::{self, WireTypeId},
    decoder::{DecodedFrameRef, Decoder},
    schema::{FieldDef, SchemaEntry},
    types::{FieldType, FieldValue, FieldValueRef, InternedString},
};
use std::collections::{BTreeSet, HashMap, HashSet};
use std::io::{self, Write};

use crate::MapsEntry;

/// Schema name for proc maps events.
pub const PROC_MAPS_EVENT_NAME: &str = "ProcMapsEntry";
/// Schema name for symbol table events.
pub const SYMBOL_TABLE_EVENT_NAME: &str = "SymbolTableEntry";

fn proc_maps_schema() -> SchemaEntry {
    SchemaEntry {
        name: PROC_MAPS_EVENT_NAME.to_string(),
        has_timestamp: false,
        fields: vec![
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
        ],
    }
}

fn symbol_table_schema() -> SchemaEntry {
    SchemaEntry {
        name: SYMBOL_TABLE_EVENT_NAME.to_string(),
        has_timestamp: false,
        fields: vec![
            FieldDef {
                name: "base_addr".into(),
                field_type: FieldType::Varint,
            },
            FieldDef {
                name: "size".into(),
                field_type: FieldType::Varint,
            },
            FieldDef {
                name: "symbol_name".into(),
                field_type: FieldType::PooledString,
            },
        ],
    }
}

/// Write ProcMapsEntry events to a writer using low-level codec functions.
///
/// Writes the schema frame followed by one event per entry.
pub fn encode_proc_maps(
    type_id: WireTypeId,
    entries: &[MapsEntry],
    w: &mut impl Write,
) -> io::Result<()> {
    codec::encode_schema(type_id, &proc_maps_schema(), w)?;
    for e in entries {
        codec::encode_event(
            type_id,
            None,
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

/// Symbolize a trace: read the input, resolve addresses from `ProcMapsEntry`
/// events and `StackFrames` data, and write the original trace plus
/// `SymbolTableEntry` events to the output.
///
/// The input trace is copied verbatim to the output, with symbol data appended.
pub fn symbolize_trace(input: &[u8], output: &mut impl Write) -> io::Result<()> {
    let mut maps: Vec<MapsEntry> = Vec::new();
    let mut addresses: BTreeSet<u64> = BTreeSet::new();
    let mut proc_maps_type_id: Option<WireTypeId> = None;

    let mut decoder = Decoder::new(input)
        .ok_or_else(|| io::Error::new(io::ErrorKind::InvalidData, "invalid trace header"))?;

    while let Ok(Some(frame)) = decoder.next_frame_ref() {
        match frame {
            DecodedFrameRef::Schema(ref entry) if entry.name == PROC_MAPS_EVENT_NAME => {
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
    let mut pool_entries: Vec<codec::PoolEntry> = Vec::new();
    let mut string_to_id: HashMap<String, u32> = HashMap::new();
    let mut seen_base_addrs: HashSet<u64> = HashSet::new();
    let mut next_pool_id: u32 = 0;
    let mut symbol_events: Vec<(u64, InternedString)> = Vec::new();

    for &addr in &addresses {
        let symbols = crate::resolve_symbols_with_maps(addr, &symbolizer, &maps);
        for info in symbols {
            let Some(name) = info.name else { continue };
            if !seen_base_addrs.insert(info.base_addr) {
                continue;
            }

            let pool_id = *string_to_id.entry(name.clone()).or_insert_with(|| {
                let id = next_pool_id;
                next_pool_id += 1;
                pool_entries.push(codec::PoolEntry {
                    pool_id: id,
                    data: name.into_bytes(),
                });
                id
            });

            symbol_events.push((info.base_addr, InternedString::from_raw(pool_id)));
        }
    }

    // Write: original trace verbatim, then append symbol data.
    output.write_all(input)?;

    if !symbol_events.is_empty() {
        codec::encode_string_pool(&pool_entries, output)?;

        let type_id = WireTypeId(0xFFFE);
        codec::encode_schema(type_id, &symbol_table_schema(), output)?;
        for (base_addr, symbol_id) in &symbol_events {
            codec::encode_event(
                type_id,
                None,
                &[
                    FieldValue::Varint(*base_addr),
                    FieldValue::Varint(0),
                    FieldValue::PooledString(*symbol_id),
                ],
                output,
            )?;
        }
    }

    Ok(())
}

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
    use dial9_trace_format::{
        codec::HEADER_SIZE,
        decoder::{DecodedFrame, Decoder},
        encoder::Encoder,
    };

    fn make_encoder_with_proc_maps(maps: &[MapsEntry]) -> Vec<u8> {
        let mut enc = Encoder::new();
        let data = enc.finish();
        let mut buf = data;
        encode_proc_maps(WireTypeId(100), maps, &mut buf).unwrap();
        buf
    }

    #[test]
    fn symbolize_empty_trace() {
        let input = Encoder::new().finish();
        let mut output = Vec::new();
        symbolize_trace(&input, &mut output).unwrap();
        assert_eq!(input, output);
    }

    #[test]
    fn symbolize_no_proc_maps_copies_verbatim() {
        let mut buf = Encoder::new().finish();
        // Write a StackFrames event using low-level codec
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
        assert_eq!(buf, output);
    }

    #[test]
    fn symbolize_no_stack_frames_copies_verbatim() {
        let input = make_encoder_with_proc_maps(&[MapsEntry {
            start: 0x1000,
            end: 0x2000,
            file_offset: 0,
            path: "/bin/test".into(),
        }]);
        let mut output = Vec::new();
        symbolize_trace(&input, &mut output).unwrap();
        assert_eq!(input, output);
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
        assert!(output.len() >= buf.len());
        let mut dec = Decoder::new(&output).unwrap();
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

        assert!(
            output.len() > buf.len(),
            "expected symbol data to be appended"
        );

        let mut dec = Decoder::new(&output).unwrap();
        let frames = dec.decode_all();
        let has_string_pool = frames
            .iter()
            .any(|f| matches!(f, DecodedFrame::StringPool(_)));
        let has_symbol_schema = frames
            .iter()
            .any(|f| matches!(f, DecodedFrame::Schema(s) if s.name == SYMBOL_TABLE_EVENT_NAME));
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
            &[codec::PoolEntry {
                pool_id: 0,
                data: b"my_function".to_vec(),
            }],
            &mut buf,
        )
        .unwrap();
        let tid = WireTypeId(0);
        codec::encode_schema(tid, &symbol_table_schema(), &mut buf).unwrap();
        codec::encode_event(
            tid,
            None,
            &[
                FieldValue::Varint(0x1000),
                FieldValue::Varint(256),
                FieldValue::PooledString(InternedString::from_raw(0)),
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
        } else {
            panic!("expected event frame");
        }
        assert_eq!(
            dec.string_pool().get(InternedString::from_raw(0)),
            Some("my_function")
        );
    }
}
