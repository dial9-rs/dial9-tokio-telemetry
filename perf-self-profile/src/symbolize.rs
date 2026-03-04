//! Minimal symbol resolution using `blazesym` with `backtrace` fallback.

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

        // Fallback: use backtrace::resolve for addresses not in /proc/self/maps
        // (e.g. vdso, or addresses that blazesym couldn't symbolize)
        resolve_with_backtrace(addr)
    })
}

/// Fallback symbol resolution using the `backtrace` crate.
/// Handles addresses that blazesym can't resolve (e.g. vdso, JIT code).
fn resolve_with_backtrace(addr: u64) -> SymbolInfo {
    let mut info = EMPTY;
    backtrace::resolve(addr as *mut std::ffi::c_void, |sym| {
        if info.name.is_some() {
            return; // take only the first resolution
        }
        info.name = sym.name().map(|n| n.to_string());
        info.base_addr = sym.addr().map_or(0, |a| a as u64);
        info.offset = addr.saturating_sub(info.base_addr);
        if let Some(filename) = sym.filename() {
            info.code_info = Some(CodeInfo {
                file: filename.to_string_lossy().into_owned(),
                line: sym.lineno(),
                column: sym.colno().map(|c| c as u16),
            });
        }
    });
    info
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

    #[inline(never)]
    fn known_function_for_test() -> u64 {
        42
    }

    /// Verify that resolve_symbol resolves a known function in the current process.
    #[test]
    fn resolve_symbol_finds_known_function() {
        let addr = known_function_for_test as *const () as u64;
        let info = resolve_symbol(addr);
        let name = info
            .name
            .expect("resolve_symbol should resolve a known function");
        assert!(
            name.contains("known_function_for_test"),
            "expected symbol containing 'known_function_for_test', got: {name}"
        );
    }

    /// Verify that a bogus kernel-space address doesn't panic and returns no name.
    #[test]
    fn resolve_symbol_bogus_address_returns_no_name() {
        let info = resolve_symbol(0xffff_ffff_dead_beef);
        assert!(info.name.is_none(), "kernel address should not resolve");
    }

    /// Verify that the backtrace fallback resolves real instruction pointers
    /// (as would come from perf_event callchains).
    #[test]
    fn backtrace_fallback_resolves_real_instruction_pointer() {
        // Capture a real IP from the current call stack
        let mut real_ip = 0u64;
        backtrace::trace(|frame| {
            if real_ip == 0 {
                real_ip = frame.ip() as u64;
            }
            false
        });
        assert_ne!(real_ip, 0, "should capture a real IP");
        let info = resolve_with_backtrace(real_ip);
        assert!(
            info.name.is_some(),
            "backtrace should resolve a real instruction pointer, got None for {:#x}",
            real_ip
        );
    }
}
