//! Minimal symbol resolution using `blazesym`.

use blazesym::symbolize::{Input, Symbolizer, source};
use std::cell::RefCell;
use std::fs;

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

/// A resolved symbol name and its base address.
#[derive(Debug, Clone)]
pub struct SymbolInfo {
    /// Demangled or raw symbol name, if found.
    pub name: Option<String>,
    /// Base address of the symbol (function start).
    pub base_addr: u64,
    /// The shared object (or executable) containing this address.
    pub object: Option<String>,
    /// Offset from the symbol base.
    pub offset: u64,
}

const EMPTY: SymbolInfo = SymbolInfo {
    name: None,
    base_addr: 0,
    object: None,
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
                    return SymbolInfo {
                        name: Some(sym.name.to_string()),
                        base_addr: sym.addr,
                        object: sym
                            .code_info
                            .as_ref()
                            .map(|c| format!("{}:{}", c.to_path().display(), c.line.unwrap_or(0))),
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
    fn parse_maps_line_malformed() {
        assert!(parse_maps_line("garbage").is_none());
        assert!(parse_maps_line("").is_none());
        assert!(parse_maps_line("not-hex r-xp 00000000 08:01 1234 /foo").is_none());
    }
}
