//! Offline symbolizer: resolves raw stack frame addresses in a trace using
//! captured `/proc/self/maps` data.
//!
//! Reads a trace containing `ProcMapsEntry` events and `StackFrames` fields,
//! resolves addresses via blazesym, and appends `SymbolTableEntry` events
//! (with a `StringPool` frame for symbol names).

use blazesym::symbolize::{Input, Symbolized, Symbolizer, source};
use dial9_trace_format::{
    TraceEvent,
    decoder::Decoder,
    encoder::Encoder,
    types::{FieldValueRef, InternedString},
};
use std::collections::BTreeSet;
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

/// Write ProcMapsEntry events to an encoder.
pub fn encode_proc_maps(
    entries: &[MapsEntry],
    encoder: &mut Encoder<impl Write>,
) -> io::Result<()> {
    for e in entries {
        encoder.write(&ProcMapsEntry {
            timestamp_ns: 0,
            start: e.start,
            end: e.end,
            file_offset: e.file_offset,
            path: e.path.clone(),
        })?;
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

    let mut decoder = Decoder::new(input)
        .ok_or_else(|| io::Error::new(io::ErrorKind::InvalidData, "invalid trace header"))?;

    decoder
        .for_each_event(|event| {
            if event.name == proc_maps_name {
                if let Some(entry) = ProcMapsEntry::decode(event.timestamp_ns, event.fields) {
                    maps.push(MapsEntry {
                        start: entry.start,
                        end: entry.end,
                        file_offset: entry.file_offset,
                        path: entry.path.to_string(),
                    });
                }
            }
            collect_stack_frame_addresses(event.fields, &mut addresses);
        })
        .map_err(|e| io::Error::new(io::ErrorKind::InvalidData, e.to_string()))?;

    if addresses.is_empty() || maps.is_empty() {
        return Ok(());
    }

    write_symbol_data(decoder, &addresses, &maps, output)
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

    decoder
        .for_each_event(|event| {
            collect_stack_frame_addresses(event.fields, &mut addresses);
        })
        .map_err(|e| io::Error::new(io::ErrorKind::InvalidData, e.to_string()))?;

    if addresses.is_empty() {
        return Ok(());
    }

    write_symbol_data(decoder, &addresses, maps, output)
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

fn write_symbol_data(
    decoder: Decoder<'_>,
    addresses: &BTreeSet<u64>,
    maps: &[MapsEntry],
    output: &mut impl Write,
) -> io::Result<()> {
    let mut encoder = decoder.into_encoder(output);
    let symbolizer = Symbolizer::new();

    // Partition addresses into kernel vs userspace, group userspace by mapping.
    let mut kernel_addrs: Vec<u64> = Vec::new();
    // (mapping_index, file_offset, original_addr)
    let mut user_groups: std::collections::HashMap<usize, Vec<(u64, u64)>> =
        std::collections::HashMap::new();

    for &addr in addresses {
        if addr >= crate::USER_ADDR_LIMIT {
            kernel_addrs.push(addr);
        } else {
            for (i, entry) in maps.iter().enumerate() {
                if addr >= entry.start && addr < entry.end {
                    let offset = addr - entry.start + entry.file_offset;
                    user_groups.entry(i).or_default().push((offset, addr));
                    break;
                }
            }
        }
    }

    // Batch-resolve kernel addresses.
    if !kernel_addrs.is_empty() {
        let src = source::Source::Kernel(source::Kernel {
            kallsyms: blazesym::MaybeDefault::Default,
            vmlinux: blazesym::MaybeDefault::None,
            kaslr_offset: Some(0),
            debug_syms: false,
            _non_exhaustive: (),
        });
        if let Ok(results) = symbolizer.symbolize(&src, Input::AbsAddr(&kernel_addrs)) {
            write_symbolized_batch(&results, &kernel_addrs, &mut encoder)?;
        } else {
            // Fallback: emit unresolved kernel placeholders.
            for &addr in &kernel_addrs {
                let name = format!("[kernel] {:#x}", addr);
                let symbol_name = encoder.intern_string(&name)?;
                let source_file = encoder.intern_string("")?;
                encoder.write(&SymbolTableEntry {
                    timestamp_ns: 0,
                    addr,
                    size: 0,
                    symbol_name,
                    inline_depth: 0,
                    source_file,
                    source_line: 0,
                })?;
            }
        }
    }

    // Batch-resolve per ELF mapping.
    for (map_idx, offsets_and_addrs) in &user_groups {
        let entry = &maps[*map_idx];
        let offsets: Vec<u64> = offsets_and_addrs.iter().map(|(o, _)| *o).collect();
        let addrs: Vec<u64> = offsets_and_addrs.iter().map(|(_, a)| *a).collect();
        let src = source::Source::Elf(source::Elf::new(&entry.path));
        if let Ok(results) = symbolizer.symbolize(&src, Input::FileOffset(&offsets)) {
            write_symbolized_batch(&results, &addrs, &mut encoder)?;
        }
    }

    Ok(())
}

/// Write a batch of symbolization results, borrowing symbol names directly
/// from the `Symbolized` results to avoid re-allocating strings.
fn write_symbolized_batch(
    results: &[Symbolized<'_>],
    addrs: &[u64],
    encoder: &mut Encoder<impl Write>,
) -> io::Result<()> {
    for (symbolized, &addr) in results.iter().zip(addrs) {
        let Some(sym) = symbolized.as_sym() else {
            continue;
        };
        let symbol_name = encoder.intern_string(&sym.name)?;
        let (source_file, source_line) = intern_code_info(sym.code_info.as_deref(), encoder)?;
        encoder.write(&SymbolTableEntry {
            timestamp_ns: 0,
            addr,
            size: 0,
            symbol_name,
            inline_depth: 0,
            source_file,
            source_line,
        })?;
        for (depth, inlined) in sym.inlined.iter().enumerate() {
            let symbol_name = encoder.intern_string(&inlined.name)?;
            let (source_file, source_line) = intern_code_info(inlined.code_info.as_ref(), encoder)?;
            encoder.write(&SymbolTableEntry {
                timestamp_ns: 0,
                addr,
                size: 0,
                symbol_name,
                inline_depth: (depth + 1) as u64,
                source_file,
                source_line,
            })?;
        }
    }
    Ok(())
}

fn intern_code_info(
    code_info: Option<&blazesym::symbolize::CodeInfo<'_>>,
    encoder: &mut Encoder<impl Write>,
) -> io::Result<(InternedString, u64)> {
    match code_info {
        Some(ci) => {
            let file = match &ci.dir {
                Some(dir) => dir
                    .join(ci.file.as_ref() as &std::path::Path)
                    .to_string_lossy()
                    .into_owned(),
                None => ci.file.to_string_lossy().into_owned(),
            };
            let interned = encoder.intern_string(&file)?;
            Ok((interned, ci.line.unwrap_or(0) as u64))
        }
        None => {
            let interned = encoder.intern_string("")?;
            Ok((interned, 0))
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use dial9_trace_format::{
        decoder::{DecodedFrame, DecodedFrameRef, Decoder},
        encoder::Encoder,
        schema::FieldDef,
        types::{FieldType, FieldValue},
    };

    fn make_encoder_with_proc_maps(maps: &[MapsEntry]) -> Vec<u8> {
        let mut enc = Encoder::new();
        encode_proc_maps(maps, &mut enc).unwrap();
        enc.finish()
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
        let mut enc = Encoder::new();
        let schema = enc
            .register_schema(
                "Ev",
                vec![FieldDef {
                    name: "frames".into(),
                    field_type: FieldType::StackFrames,
                }],
            )
            .unwrap();
        enc.write_event(
            &schema,
            &[FieldValue::Varint(0), FieldValue::StackFrames(vec![0x1000])],
        )
        .unwrap();
        let buf = enc.finish();

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
        let mut enc = Encoder::new();
        let schema = enc
            .register_schema(
                "Ev",
                vec![FieldDef {
                    name: "frames".into(),
                    field_type: FieldType::StackFrames,
                }],
            )
            .unwrap();
        enc.write_event(
            &schema,
            &[
                FieldValue::Varint(0),
                FieldValue::StackFrames(vec![0x55a4b2c01000]),
            ],
        )
        .unwrap();
        encode_proc_maps(
            &[MapsEntry {
                start: 0x55a4b2c00000,
                end: 0x55a4b2c05000,
                file_offset: 0x1000,
                path: "/nonexistent/binary".into(),
            }],
            &mut enc,
        )
        .unwrap();
        let buf = enc.finish();

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

        let mut enc = Encoder::new();
        let schema = enc
            .register_schema(
                "Ev",
                vec![FieldDef {
                    name: "frames".into(),
                    field_type: FieldType::StackFrames,
                }],
            )
            .unwrap();
        enc.write_event(
            &schema,
            &[FieldValue::Varint(0), FieldValue::StackFrames(vec![addr])],
        )
        .unwrap();
        encode_proc_maps(&raw_maps, &mut enc).unwrap();
        let buf = enc.finish();

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

        let mut enc = Encoder::new();
        let schema = enc
            .register_schema(
                "Ev",
                vec![FieldDef {
                    name: "frames".into(),
                    field_type: FieldType::StackFrames,
                }],
            )
            .unwrap();
        enc.write_event(
            &schema,
            &[FieldValue::Varint(0), FieldValue::StackFrames(vec![addr])],
        )
        .unwrap();
        let buf = enc.finish();

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
        let mut enc = Encoder::new();
        encode_proc_maps(std::slice::from_ref(&entry), &mut enc).unwrap();
        let buf = enc.finish();

        let mut dec = Decoder::new(&buf).unwrap();
        let frames = dec.decode_all_ref();
        assert_eq!(frames.len(), 2);
        if let DecodedFrameRef::Event {
            timestamp_ns,
            values,
            ..
        } = &frames[1]
        {
            let decoded = ProcMapsEntry::decode(*timestamp_ns, values).unwrap();
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
}
