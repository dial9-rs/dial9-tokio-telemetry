//! Offline symbolizer: resolves raw stack frame addresses in a trace using
//! captured `/proc/self/maps` data.
//!
//! Reads a trace containing `ProcMapsEntry` events and `StackFrames` fields,
//! resolves addresses via blazesym, and appends `SymbolTableEntry` events
//! (with a `StringPool` frame for symbol names).

use blazesym::symbolize::{Input, Symbolized, Symbolizer, source};
use dial9_trace_format::{
    decoder::Decoder,
    encoder::Encoder,
    types::{FieldValueRef, InternedString},
};
use std::collections::{BTreeSet, HashMap};
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
    // TODO: avoid recreating the Symbolizer here every time. This is a little non trivial because of threading issues and Symbolizer being !Send and !Sync.
    // We need to basically have a background symbolization thread.
    let symbolizer = Symbolizer::new();

    // Partition addresses into kernel vs userspace, group userspace by mapping.
    let mut kernel_addrs: Vec<u64> = Vec::new();
    // (mapping_index, file_offset, original_addr)
    let mut user_groups: HashMap<usize, Vec<(u64, u64)>> = HashMap::new();

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
                let source_file = encoder.intern_string("kernel")?;
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
        match symbolizer.symbolize(&src, Input::FileOffset(&offsets)) {
            Ok(results) => {
                write_symbolized_batch(&results, &addrs, &mut encoder)?;
            }
            Err(err) => {
                tracing::warn!(
                    path = %entry.path,
                    count = addrs.len(),
                    error = %err,
                    "failed to symbolize batch for ELF mapping, using placeholders"
                );
                for &addr in &addrs {
                    let name = format!("[symbolize-failed] {:#x}", addr);
                    let symbol_name = encoder.intern_string(&name)?;
                    let source_file = encoder.intern_string(&entry.path)?;
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
            let interned = match &ci.dir {
                Some(dir) => {
                    // todo: avoid allocations here
                    let joined = dir.join(ci.file.as_ref() as &std::path::Path);
                    encoder.intern_string(&joined.to_string_lossy())?
                }
                None => encoder.intern_string(&ci.file.to_string_lossy())?,
            };
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
        decoder::{DecodedFrame, Decoder},
        encoder::Encoder,
        schema::FieldDef,
        types::{FieldType, FieldValue},
    };

    #[cfg(target_os = "linux")]
    #[test]
    fn symbolize_with_maps_produces_symbol_events() {
        use dial9_trace_format::TraceEvent;

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
            .register_schema(
                "Ev",
                vec![FieldDef {
                    name: "count".into(),
                    field_type: FieldType::Varint,
                }],
            )
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
        symbolize_trace_with_maps(&buf, &[], &mut output).unwrap();
        // Addresses exist but none match any mapping, so no symbols are emitted.
        assert!(output.is_empty());
    }

    #[test]
    fn symbolize_emits_placeholders_when_elf_missing() {
        use dial9_trace_format::TraceEvent;

        // Pick a userspace address that falls within our fake mapping.
        let addr: u64 = 0x1000;
        let fake_path = "/nonexistent/fake.so";

        let maps = vec![MapsEntry {
            start: 0x0,
            end: 0x2000,
            file_offset: 0,
            path: fake_path.to_string(),
        }];

        // Build a minimal trace containing a single StackFrames event with our address.
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
        symbolize_trace_with_maps(&buf, &maps, &mut output).unwrap();

        assert!(!output.is_empty(), "expected symbol data to be written");

        // Decode the output and verify we got placeholder symbols.
        let mut combined = buf.clone();
        combined.extend_from_slice(&output);
        let mut dec = Decoder::new(&combined).unwrap();
        let frames = dec.decode_all();

        let has_symbol_schema = frames.iter().any(
            |f| matches!(f, DecodedFrame::Schema(s) if s.name == SymbolTableEntry::event_name()),
        );
        assert!(
            has_symbol_schema,
            "expected SymbolTableEntry schema in output"
        );

        // There should be at least one event frame beyond the input schema+event.
        let event_count = frames
            .iter()
            .filter(|f| matches!(f, DecodedFrame::Event { .. }))
            .count();
        // We expect the original input event + at least one SymbolTableEntry event.
        assert!(
            event_count >= 2,
            "expected at least 2 events (input + placeholder), got {}",
            event_count
        );

        // Verify the string pool contains the placeholder name and source file.
        let pool = dec.string_pool();
        let pool_strings: Vec<&str> = (0..100)
            .filter_map(|i| pool.get(InternedString::from_raw(i)))
            .collect();
        let has_placeholder = pool_strings
            .iter()
            .any(|s| s.starts_with("[symbolize-failed]") && s.contains(&format!("{:#x}", addr)));
        assert!(
            has_placeholder,
            "expected '[symbolize-failed] 0x1000' in string pool, got: {:?}",
            pool_strings
        );
        let has_source_file = pool_strings.contains(&fake_path);
        assert!(
            has_source_file,
            "expected source file '{}' in string pool, got: {:?}",
            fake_path, pool_strings
        );
    }
}
