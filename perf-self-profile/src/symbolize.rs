//! Minimal symbol resolution using `blazesym`.

use blazesym::symbolize::{Input, Symbolizer, source};
use std::cell::RefCell;
use std::fs;

use crate::USER_ADDR_LIMIT;

struct SymbolizerState {
    symbolizer: Symbolizer,
    /// Map from address range to (path, base_addr)
    mappings: Vec<(u64, u64, String, u64)>,
}

thread_local! {
    static SYMBOLIZER: RefCell<Option<SymbolizerState>> = const { RefCell::new(None) };
}

fn parse_maps() -> Vec<(u64, u64, String, u64)> {
    let maps = fs::read_to_string("/proc/self/maps").unwrap_or_default();
    let mut result = Vec::new();

    for line in maps.lines() {
        if let Some(entry) = parse_maps_line(line) {
            result.push(entry);
        }
    }

    result
}

fn parse_maps_line(line: &str) -> Option<(u64, u64, String, u64)> {
    let mut parts = line.split_whitespace();
    let addr_range = parts.next()?;
    let perms = parts.next()?;
    if !perms.contains('x') {
        return None;
    }
    let offset_str = parts.next()?;
    let _dev = parts.next()?;
    let _inode = parts.next()?;
    let path = parts.next()?;
    if !path.starts_with('/') {
        return None;
    }

    let (start_str, end_str) = addr_range.split_once('-')?;
    let start = u64::from_str_radix(start_str, 16).ok()?;
    let end = u64::from_str_radix(end_str, 16).ok()?;
    let offset = u64::from_str_radix(offset_str, 16).ok()?;

    Some((start, end, path.to_string(), offset))
}

/// Source location information for a resolved symbol.
#[derive(Debug, Clone)]
pub struct CodeInfo {
    /// Source file path (includes directory when available from debug info).
    pub file: String,
    /// Line number within the source file, if available.
    pub line: Option<u32>,
    /// Column number within the source file, if available.
    pub column: Option<u16>,
}

/// A resolved symbol name and its base address.
#[derive(Debug, Clone)]
pub struct SymbolInfo {
    /// Demangled or raw symbol name, if found.
    pub name: Option<String>,
    /// Base address of the symbol (function start).
    pub base_addr: u64,
    /// Source location (file, line, column) for this symbol, if available.
    pub code_info: Option<CodeInfo>,
    /// Offset from the symbol base.
    pub offset: u64,
}

const EMPTY: SymbolInfo = SymbolInfo {
    name: None,
    base_addr: 0,
    code_info: None,
    offset: 0,
};

/// Resolve an instruction pointer to a symbol name.
pub fn resolve_symbol(addr: u64) -> SymbolInfo {
    SYMBOLIZER.with(|cell| {
        let mut opt = cell.borrow_mut();
        if opt.is_none() {
            *opt = Some(SymbolizerState {
                symbolizer: Symbolizer::new(),
                mappings: parse_maps(),
            });
        }
        let state = opt.as_ref().unwrap();

        // Kernel addresses are >= USER_ADDR_LIMIT
        if addr >= USER_ADDR_LIMIT {
            let src = source::Source::Kernel(source::Kernel {
                kallsyms: blazesym::MaybeDefault::Default,
                vmlinux: blazesym::MaybeDefault::None,
                kaslr_offset: Some(0),
                debug_syms: false,
                _non_exhaustive: (),
            });
            if let Ok(results) = state.symbolizer.symbolize(&src, Input::AbsAddr(&[addr]))
                && !results.is_empty()
                && let Some(sym) = results[0].as_sym()
            {
                return SymbolInfo {
                    name: Some(sym.name.to_string()),
                    base_addr: sym.addr,
                    code_info: None,
                    offset: addr.saturating_sub(sym.addr),
                };
            }
            // TODO: code_info should be available from kernel DWARF debug symbols
            return SymbolInfo {
                name: Some(format!("[kernel] {:#x}", addr)),
                base_addr: addr,
                code_info: None,
                offset: 0,
            };
        }

        // Find which mapping contains this address
        for (start, end, path, file_offset) in &state.mappings {
            if addr >= *start && addr < *end {
                let offset = addr - start + file_offset;
                let src = source::Source::Elf(source::Elf::new(path));
                if let Ok(results) = state
                    .symbolizer
                    .symbolize(&src, Input::FileOffset(&[offset]))
                    && !results.is_empty()
                    && let Some(sym) = results[0].as_sym()
                {
                    // TODO: can we refactor this to borrow symbols?
                    return SymbolInfo {
                        name: Some(sym.name.to_string()),
                        base_addr: sym.addr,
                        code_info: sym.code_info.as_ref().map(|c| {
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
                        }),
                        offset: addr.saturating_sub(sym.addr),
                    };
                }
                break;
            }
        }

        EMPTY
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_maps_line_executable() {
        let line = "55a4b2c00000-55a4b2c05000 r-xp 00001000 08:01 1234 /usr/bin/foo";
        let (start, end, path, offset) = parse_maps_line(line).unwrap();
        assert_eq!(start, 0x55a4b2c00000);
        assert_eq!(end, 0x55a4b2c05000);
        assert_eq!(path, "/usr/bin/foo");
        assert_eq!(offset, 0x1000);
    }

    #[test]
    fn parse_maps_line_non_executable() {
        let line = "7f1234000000-7f1234001000 r--p 00000000 08:01 1234 /usr/lib/foo.so";
        assert!(parse_maps_line(line).is_none());
    }

    #[test]
    fn parse_maps_line_no_path() {
        let line = "7ffd12300000-7ffd12321000 r-xp 00000000 00:00 0 [vdso]";
        assert!(parse_maps_line(line).is_none());
    }

    #[test]
    fn parse_maps_line_anon() {
        let line = "7f1234000000-7f1234001000 r-xp 00000000 00:00 0";
        assert!(parse_maps_line(line).is_none());
    }

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

    #[test]
    fn parse_maps_line_malformed() {
        assert!(parse_maps_line("garbage").is_none());
        assert!(parse_maps_line("").is_none());
        assert!(parse_maps_line("not-hex r-xp 00000000 08:01 1234 /foo").is_none());
    }
}
