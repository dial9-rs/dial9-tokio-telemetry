//! Symbol resolution using blazesym (Linux).

use blazesym::symbolize::{Input, Symbolizer, source};
use std::cell::RefCell;

use super::USER_ADDR_LIMIT;
use crate::symbolize::{CodeInfo, MapsEntry, SymbolInfo, read_proc_maps};

struct SymbolizerState {
    symbolizer: Symbolizer,
    mappings: Vec<MapsEntry>,
}

thread_local! {
    static SYMBOLIZER: RefCell<Option<SymbolizerState>> = const { RefCell::new(None) };
}

const EMPTY: SymbolInfo = SymbolInfo {
    name: None,
    base_addr: 0,
    code_info: None,
    offset: 0,
};

/// Resolve an instruction pointer to a symbol name using the current process's mappings.
pub fn resolve_symbol(addr: u64) -> SymbolInfo {
    SYMBOLIZER.with(|cell| {
        let mut opt = cell.borrow_mut();
        if opt.is_none() {
            *opt = Some(SymbolizerState {
                symbolizer: Symbolizer::new(),
                mappings: read_proc_maps(),
            });
        }
        let state = opt.as_ref().unwrap();
        resolve_symbol_with_maps(addr, &state.symbolizer, &state.mappings)
    })
}

/// Resolve an instruction pointer using the provided mappings.
///
/// Returns the outermost symbol only. For inlined function support, use
/// [`resolve_symbols_with_maps`].
pub fn resolve_symbol_with_maps(
    addr: u64,
    symbolizer: &Symbolizer,
    mappings: &[MapsEntry],
) -> SymbolInfo {
    resolve_symbols_with_maps(addr, symbolizer, mappings)
        .into_iter()
        .next()
        .unwrap_or(EMPTY)
}

/// Resolve an instruction pointer to all symbols at that address, including
/// inlined functions.
///
/// Returns symbols from outermost to innermost. For a non-inlined call this
/// returns a single entry. For an address inside `f -> g (inlined) -> h (inlined)`,
/// returns `[f, g, h]`.
///
/// Returns an empty vec if the address cannot be resolved.
pub fn resolve_symbols_with_maps(
    addr: u64,
    symbolizer: &Symbolizer,
    mappings: &[MapsEntry],
) -> Vec<SymbolInfo> {
    // Kernel addresses are >= USER_ADDR_LIMIT
    if addr >= USER_ADDR_LIMIT {
        let src = source::Source::Kernel(source::Kernel {
            kallsyms: blazesym::MaybeDefault::Default,
            vmlinux: blazesym::MaybeDefault::None,
            kaslr_offset: Some(0),
            debug_syms: false,
            _non_exhaustive: (),
        });
        if let Ok(results) = symbolizer.symbolize(&src, Input::AbsAddr(&[addr]))
            && !results.is_empty()
            && let Some(sym) = results[0].as_sym()
        {
            let mut symbols = Vec::with_capacity(1 + sym.inlined.len());
            symbols.push(SymbolInfo {
                name: Some(sym.name.to_string()),
                base_addr: sym.addr,
                code_info: convert_code_info(sym.code_info.as_deref()),
                offset: addr.saturating_sub(sym.addr),
            });
            for inlined in sym.inlined.iter() {
                symbols.push(SymbolInfo {
                    name: Some(inlined.name.to_string()),
                    base_addr: sym.addr,
                    code_info: convert_code_info(inlined.code_info.as_ref()),
                    offset: addr.saturating_sub(sym.addr),
                });
            }
            return symbols;
        }
        return vec![SymbolInfo {
            name: Some(format!("[kernel] {:#x}", addr)),
            base_addr: addr,
            code_info: None,
            offset: 0,
        }];
    }

    for entry in mappings {
        if addr >= entry.start && addr < entry.end {
            let offset = addr - entry.start + entry.file_offset;
            let src = source::Source::Elf(source::Elf::new(&entry.path));
            if let Ok(results) = symbolizer.symbolize(&src, Input::FileOffset(&[offset]))
                && !results.is_empty()
                && let Some(sym) = results[0].as_sym()
            {
                let mut symbols = Vec::with_capacity(1 + sym.inlined.len());
                symbols.push(SymbolInfo {
                    name: Some(sym.name.to_string()),
                    base_addr: sym.addr,
                    code_info: convert_code_info(sym.code_info.as_deref()),
                    offset: addr.saturating_sub(sym.addr),
                });
                for inlined in sym.inlined.iter() {
                    symbols.push(SymbolInfo {
                        name: Some(inlined.name.to_string()),
                        base_addr: sym.addr,
                        code_info: convert_code_info(inlined.code_info.as_ref()),
                        offset: addr.saturating_sub(sym.addr),
                    });
                }
                return symbols;
            }
            break;
        }
    }
    Vec::new()
}

fn convert_code_info(c: Option<&blazesym::symbolize::CodeInfo<'_>>) -> Option<CodeInfo> {
    c.map(|c| {
        let file = match &c.dir {
            Some(dir) => dir
                .join(c.file.as_ref() as &std::path::Path)
                .to_string_lossy()
                .into_owned(),
            None => c.file.to_string_lossy().into_owned(),
        };
        CodeInfo {
            file,
            line: c.line,
            column: c.column,
        }
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;

    #[test]
    fn resolve_kernel_symbol_returns_name() {
        // Pick a well-known kernel symbol from /proc/kallsyms
        let kallsyms = fs::read_to_string("/proc/kallsyms").unwrap_or_default();
        let entry = kallsyms
            .lines()
            .find(|l| {
                let mut parts = l.split_whitespace();
                parts.next(); // addr
                parts.next(); // type
                parts.next() == Some("schedule")
            })
            .expect("schedule not found in kallsyms");
        let addr = u64::from_str_radix(entry.split_whitespace().next().unwrap(), 16).unwrap();
        let info = resolve_symbol(addr);
        assert_eq!(info.name.as_deref(), Some("schedule"));
    }
}
