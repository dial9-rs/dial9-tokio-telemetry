//! Shared types for symbol resolution: memory mappings, symbol info, and
//! `/proc/self/maps` parsing.

use std::fs;

/// A single executable memory mapping parsed from `/proc/self/maps`.
#[derive(Debug, Clone, PartialEq)]
pub struct MapsEntry {
    /// Start address of the mapping.
    pub start: u64,
    /// End address of the mapping.
    pub end: u64,
    /// File offset of the mapping.
    pub file_offset: u64,
    /// Path to the mapped file.
    pub path: String,
}

/// Read the current process's executable memory mappings from `/proc/self/maps`.
///
/// Returns only executable (`r-xp`) mappings with file-backed paths (starting with `/`).
pub fn read_proc_maps() -> Vec<MapsEntry> {
    parse_proc_maps(&fs::read_to_string("/proc/self/maps").unwrap_or_default())
}

/// Parse `/proc/self/maps` content into structured entries.
///
/// Filters to executable, file-backed mappings only.
pub fn parse_proc_maps(maps_content: &str) -> Vec<MapsEntry> {
    maps_content.lines().filter_map(parse_maps_line).collect()
}

fn parse_maps_line(line: &str) -> Option<MapsEntry> {
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
    let file_offset = u64::from_str_radix(offset_str, 16).ok()?;

    Some(MapsEntry {
        start,
        end,
        file_offset,
        path: path.to_string(),
    })
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_maps_line_executable() {
        let line = "55a4b2c00000-55a4b2c05000 r-xp 00001000 08:01 1234 /usr/bin/foo";
        let entry = parse_maps_line(line).unwrap();
        assert_eq!(entry.start, 0x55a4b2c00000);
        assert_eq!(entry.end, 0x55a4b2c05000);
        assert_eq!(entry.path, "/usr/bin/foo");
        assert_eq!(entry.file_offset, 0x1000);
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

    #[test]
    fn parse_proc_maps_filters_correctly() {
        let content = "\
55a4b2c00000-55a4b2c05000 r-xp 00001000 08:01 1234 /usr/bin/foo
7f1234000000-7f1234001000 r--p 00000000 08:01 1234 /usr/lib/foo.so
7f1234100000-7f1234200000 r-xp 00000000 08:01 5678 /usr/lib/libbar.so
7ffd12300000-7ffd12321000 r-xp 00000000 00:00 0 [vdso]";
        let entries = parse_proc_maps(content);
        assert_eq!(entries.len(), 2);
        assert_eq!(entries[0].path, "/usr/bin/foo");
        assert_eq!(entries[1].path, "/usr/lib/libbar.so");
    }
}
