//! Linux offline symbolization using blazesym.

use blazesym::symbolize::{Input, Symbolized, Symbolizer, source};
use dial9_trace_format::{decoder::Decoder, encoder::Encoder};
use std::collections::{BTreeSet, HashMap};
use std::io::{self, Write};

use super::USER_ADDR_LIMIT;
use crate::MapsEntry;
use crate::offline_symbolize::SymbolTableEntry;

pub(crate) fn write_symbol_data(
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
        if addr >= USER_ADDR_LIMIT {
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
) -> io::Result<(dial9_trace_format::types::InternedString, u64)> {
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
    use crate::MapsEntry;
    use crate::offline_symbolize::{SymbolTableEntry, symbolize_trace_with_maps};
    use dial9_trace_format::{
        TraceEvent,
        decoder::{DecodedFrame, Decoder},
        encoder::Encoder,
        schema::FieldDef,
        types::{FieldType, FieldValue},
    };

    fn symbol_table_addrs(dec: &Decoder<'_>, frames: &[DecodedFrame]) -> Vec<u64> {
        let mut out = Vec::new();
        for frame in frames {
            if let DecodedFrame::Event {
                type_id, values, ..
            } = frame
                && let Some(entry) = dec.registry().get(*type_id)
                && entry.name == SymbolTableEntry::event_name()
                && let Some(FieldValue::Varint(addr)) = values.first()
            {
                out.push(*addr);
            }
        }
        out
    }

    #[test]
    fn symbolize_with_maps_handles_multi_segment_pooled_stacks() {
        let addr_a = symbolize_with_maps_handles_multi_segment_pooled_stacks as *const () as u64;
        let addr_b = symbolize_with_maps_produces_symbol_events as *const () as u64;
        let raw_maps = crate::read_proc_maps();

        // Segment 1: contains addr_a in its pool.
        let mut enc1 = Encoder::new();
        let schema1 = enc1
            .register_schema(
                "Ev",
                vec![FieldDef {
                    name: "frames".into(),
                    field_type: FieldType::PooledStackFrames,
                }],
            )
            .unwrap();
        let id_a = enc1.intern_stack_frames(&[addr_a]).unwrap();
        enc1.write_event(
            &schema1,
            &[FieldValue::Varint(0), FieldValue::PooledStackFrames(id_a)],
        )
        .unwrap();
        let seg1 = enc1.finish();

        // Segment 2: contains addr_b.
        let mut enc2 = Encoder::new();
        let schema2 = enc2
            .register_schema(
                "Ev",
                vec![FieldDef {
                    name: "frames".into(),
                    field_type: FieldType::PooledStackFrames,
                }],
            )
            .unwrap();
        let id_b = enc2.intern_stack_frames(&[addr_b]).unwrap();
        enc2.write_event(
            &schema2,
            &[FieldValue::Varint(0), FieldValue::PooledStackFrames(id_b)],
        )
        .unwrap();
        let seg2 = enc2.finish();

        let mut concatenated = seg1;
        concatenated.extend_from_slice(&seg2);

        let mut output = Vec::new();
        symbolize_trace_with_maps(&concatenated, &raw_maps, &mut output).unwrap();

        let mut combined = concatenated.clone();
        combined.extend_from_slice(&output);
        let mut dec = Decoder::new(&combined).unwrap();
        let frames = dec.decode_all();

        let addrs = symbol_table_addrs(&dec, &frames);
        assert!(
            addrs.contains(&addr_a),
            "missing SymbolTableEntry for first-segment address"
        );
        assert!(
            addrs.contains(&addr_b),
            "missing SymbolTableEntry for second-segment address"
        );
    }

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
    fn symbolize_emits_placeholders_when_elf_missing() {
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
            .filter_map(|i| pool.get(dial9_trace_format::types::InternedString::from_raw(i)))
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
