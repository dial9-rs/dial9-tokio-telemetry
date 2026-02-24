//! Minimal symbol resolution using `blazesym`.

use blazesym::symbolize::{source, Input, Symbolizer};
use std::cell::RefCell;
use std::fs;
use std::path::PathBuf;

thread_local! {
    static SYMBOLIZER: RefCell<Option<(Symbolizer, u64)>> = RefCell::new(None);
}

fn get_base_addr() -> Option<u64> {
    let maps = fs::read_to_string("/proc/self/maps").ok()?;
    let exe = fs::read_link("/proc/self/exe").ok()?;
    let exe_str = exe.to_str()?;
    
    for line in maps.lines() {
        if line.contains("r-xp") && line.contains(exe_str) {
            let addr_range = line.split_whitespace().next()?;
            let start = addr_range.split('-').next()?;
            return u64::from_str_radix(start, 16).ok();
        }
    }
    None
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

/// Resolve an instruction pointer to a symbol name.
pub fn resolve_symbol(addr: u64) -> SymbolInfo {
    SYMBOLIZER.with(|cell| {
        let mut opt = cell.borrow_mut();
        if opt.is_none() {
            let base = get_base_addr().unwrap_or(0);
            *opt = Some((Symbolizer::new(), base));
        }
        
        let (symbolizer, base) = opt.as_ref().unwrap();
        let offset = addr.saturating_sub(*base);
        let src = source::Source::Elf(source::Elf::new(PathBuf::from("/proc/self/exe")));
        let syms = symbolizer.symbolize(&src, Input::VirtOffset(&[offset]));
        
        match syms {
            Ok(results) if !results.is_empty() => {
                if let Some(sym) = results[0].as_sym() {
                    SymbolInfo {
                        name: Some(sym.name.to_string()),
                        base_addr: sym.addr,
                        object: None,
                        offset: offset.saturating_sub(sym.addr),
                    }
                } else {
                    SymbolInfo { name: None, base_addr: 0, object: None, offset: 0 }
                }
            }
            _ => SymbolInfo { name: None, base_addr: 0, object: None, offset: 0 },
        }
    })
}
