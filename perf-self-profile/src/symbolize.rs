//! Minimal symbol resolution using `blazesym`.

use blazesym::symbolize::{source, Input, Symbolizer};
use std::cell::RefCell;
use std::collections::HashMap;
use std::fs;

struct SymbolizerState {
    symbolizer: Symbolizer,
    /// Map from address range to (path, base_addr)
    mappings: Vec<(u64, u64, String, u64)>,
}

thread_local! {
    static SYMBOLIZER: RefCell<Option<SymbolizerState>> = RefCell::new(None);
}

fn parse_maps() -> Vec<(u64, u64, String, u64)> {
    let maps = fs::read_to_string("/proc/self/maps").unwrap_or_default();
    let mut result = Vec::new();
    
    for line in maps.lines() {
        let parts: Vec<&str> = line.split_whitespace().collect();
        if parts.len() < 6 || !parts[1].contains('x') {
            continue; // Skip non-executable mappings
        }
        
        let addr_range = parts[0];
        let mut range_parts = addr_range.split('-');
        let start = u64::from_str_radix(range_parts.next().unwrap(), 16).unwrap();
        let end = u64::from_str_radix(range_parts.next().unwrap(), 16).unwrap();
        let offset = u64::from_str_radix(parts[2], 16).unwrap();
        let path = parts[5].to_string();
        
        if !path.starts_with('/') {
            continue; // Skip vdso, stack, etc.
        }
        
        result.push((start, end, path, offset));
    }
    
    result
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
                if let Ok(results) = state.symbolizer.symbolize(&src, Input::FileOffset(&[offset])) {
                    if !results.is_empty() {
                        if let Some(sym) = results[0].as_sym() {
                            return SymbolInfo {
                                name: Some(sym.name.to_string()),
                                base_addr: sym.addr,
                                object: sym.code_info.as_ref().map(|c| 
                                    format!("{}:{}", c.to_path().display(), c.line.unwrap_or(0))
                                ),
                                offset: addr.saturating_sub(sym.addr),
                            };
                        }
                    }
                }
                break;
            }
        }
        
        EMPTY
    })
}
