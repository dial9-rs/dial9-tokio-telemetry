//! Parse kernel tracepoint format files and extract typed field values from raw
//! perf sample data.
//!
//! Tracepoint format files live under tracefs, typically at
//! `/sys/kernel/debug/tracing/events/<subsystem>/<event>/format`.
//!
//! # Example
//!
//! ```no_run
//! use dial9_perf_self_profile::tracepoint::TracepointDef;
//!
//! let tp = TracepointDef::from_event("sched", "sched_switch").unwrap();
//! println!("tracepoint {} has id {}", tp.name, tp.id);
//! for field in &tp.fields {
//!     println!("  {}: {} (offset={}, size={})", field.name, field.field_type, field.offset, field.size);
//! }
//! ```

use std::io::{self, Write};
use std::path::{Path, PathBuf};

use dial9_trace_format::encoder::{Encoder, Schema};
use dial9_trace_format::schema::FieldDef;
use dial9_trace_format::types::{FieldType, FieldValue};

use crate::EventSource;

/// A single field parsed from a kernel tracepoint format file.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TracepointField {
    /// Field name, e.g. `"prev_pid"`.
    pub name: String,
    /// Raw kernel type string, e.g. `"unsigned int"`, `"char[16]"`, `"pid_t"`.
    pub field_type: String,
    /// Byte offset within the raw tracepoint data.
    pub offset: usize,
    /// Size in bytes (for `__data_loc` fields this is 4 — the loc descriptor).
    pub size: usize,
    /// Whether the field is signed.
    pub signed: bool,
    /// `true` for `__data_loc` fields whose data is appended after the
    /// fixed-size record.  The 4 bytes at `offset` encode
    /// `(data_offset << 16) | data_length`.
    pub is_dynamic: bool,
}

/// A parsed kernel tracepoint definition.
#[derive(Debug, Clone)]
pub struct TracepointDef {
    /// Event name, e.g. `"sched_switch"`.
    pub name: String,
    /// Tracepoint ID for use with `perf_event_open` (`attr.config`).
    pub id: u32,
    /// Non-common fields (common fields like `common_type` are excluded).
    pub fields: Vec<TracepointField>,
}

/// Default tracefs mount point.
const TRACEFS_PATH: &str = "/sys/kernel/debug/tracing";

/// Find the tracefs mount point. Checks the default location first, then falls
/// back to `/sys/kernel/tracing` (tracefs mounted directly on newer kernels).
fn tracefs_root() -> io::Result<PathBuf> {
    let default = Path::new(TRACEFS_PATH);
    if default.exists() {
        return Ok(default.to_path_buf());
    }
    let alt = Path::new("/sys/kernel/tracing");
    if alt.exists() {
        return Ok(alt.to_path_buf());
    }
    Err(io::Error::new(
        io::ErrorKind::NotFound,
        "tracefs not found at /sys/kernel/debug/tracing or /sys/kernel/tracing",
    ))
}

impl TracepointDef {
    /// Look up a tracepoint by subsystem and event name, reading from tracefs.
    ///
    /// Reads the format file at `<tracefs>/events/<subsystem>/<event>/format`.
    pub fn from_event(subsystem: &str, event: &str) -> io::Result<Self> {
        let root = tracefs_root()?;
        let format_path = root
            .join("events")
            .join(subsystem)
            .join(event)
            .join("format");
        Self::from_format_file(&format_path)
    }

    /// Parse a tracepoint format file.
    pub fn from_format_file(path: &Path) -> io::Result<Self> {
        let contents = std::fs::read_to_string(path)?;
        Self::parse_format(&contents)
    }

    /// Parse format file contents (useful for testing without filesystem access).
    pub fn parse_format(contents: &str) -> io::Result<Self> {
        let mut name = String::new();
        let mut id: u32 = 0;
        let mut fields = Vec::new();

        for line in contents.lines() {
            let trimmed = line.trim();

            if let Some(rest) = trimmed.strip_prefix("name:") {
                name = rest.trim().to_string();
            } else if let Some(rest) = trimmed.strip_prefix("ID:") {
                id = rest.trim().parse::<u32>().map_err(|e| {
                    io::Error::new(io::ErrorKind::InvalidData, format!("bad ID: {e}"))
                })?;
            } else if let Some(rest) = trimmed.strip_prefix("field:")
                && let Some(field) = parse_field_line(rest)
            {
                // Skip common fields (kernel bookkeeping).
                if !field.name.starts_with("common_") {
                    fields.push(field);
                }
            }
        }

        if name.is_empty() {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                "format file missing 'name:' line",
            ));
        }
        if id == 0 {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                "format file missing or zero 'ID:' line",
            ));
        }

        Ok(TracepointDef { name, id, fields })
    }

    /// Extract typed field values from raw tracepoint data.
    ///
    /// Returns `(field_name, value)` pairs for each field in the definition.
    /// Returns an error if the raw data is too short for any field.
    pub fn extract_fields<'a>(&'a self, raw: &[u8]) -> io::Result<Vec<(&'a str, RawFieldValue)>> {
        let mut result = Vec::with_capacity(self.fields.len());
        for field in &self.fields {
            let end = field.offset + field.size;
            if end > raw.len() {
                return Err(io::Error::new(
                    io::ErrorKind::InvalidData,
                    format!(
                        "raw data too short for field '{}': need {} bytes, have {}",
                        field.name,
                        end,
                        raw.len()
                    ),
                ));
            }

            if field.is_dynamic {
                // __data_loc: 4 bytes at field.offset encode (offset << 16) | length
                // TODO: from_ne_bytes assumes little-endian layout — not portable to big-endian.
                let loc =
                    u32::from_ne_bytes(raw[field.offset..field.offset + 4].try_into().unwrap());
                let data_off = (loc >> 16) as usize;
                let data_len = (loc & 0xffff) as usize;
                let data_end = data_off + data_len;
                if data_end > raw.len() {
                    return Err(io::Error::new(
                        io::ErrorKind::InvalidData,
                        format!(
                            "__data_loc field '{}' points beyond raw data: off={} len={}, have {}",
                            field.name,
                            data_off,
                            data_len,
                            raw.len()
                        ),
                    ));
                }
                let s = nul_terminated_str(&raw[data_off..data_end]);
                result.push((field.name.as_str(), RawFieldValue::Str(s)));
            } else {
                let bytes = &raw[field.offset..end];
                let value = extract_field_value(bytes, field.size, field.signed, &field.field_type);
                result.push((field.name.as_str(), value));
            }
        }
        Ok(result)
    }

    /// Convert field definitions to `dial9-trace-format` [`FieldDef`]s.
    pub fn to_trace_format_fields(&self) -> Vec<FieldDef> {
        self.fields
            .iter()
            .map(kernel_field_to_trace_format)
            .collect()
    }

    /// Convert extracted field values to `dial9-trace-format` [`FieldValue`]s.
    pub fn to_trace_format_values(&self, extracted: &[(&str, RawFieldValue)]) -> Vec<FieldValue> {
        extracted
            .iter()
            .map(|(_name, raw_val)| raw_to_trace_format_value(raw_val))
            .collect()
    }

    /// Return an [`EventSource`] for use with [`SamplerConfig`](crate::SamplerConfig).
    pub fn event_source(&self) -> EventSource {
        EventSource::Tracepoint(self.id)
    }

    /// Create a [`Schema`] handle for this tracepoint.
    ///
    /// The returned handle can be passed to [`Encoder::write_event`] — it will
    /// auto-register with any encoder on first use.
    pub fn schema(&self) -> Schema {
        Schema::new(&self.name, self.to_trace_format_fields())
    }

    /// Register this tracepoint's schema with an [`Encoder`], returning a
    /// [`Schema`] handle for subsequent [`encode_raw`](Self::encode_raw) calls.
    pub fn register<W: Write>(&self, encoder: &mut Encoder<W>) -> io::Result<Schema> {
        encoder.register_schema(&self.name, self.to_trace_format_fields())
    }

    /// Extract fields from `raw` tracepoint data and write a single event into
    /// `encoder`.
    ///
    /// `schema` should have been obtained from [`register`](Self::register) or
    /// [`schema`](Self::schema).
    pub fn encode_raw<W: Write>(
        &self,
        encoder: &mut Encoder<W>,
        schema: &Schema,
        timestamp_ns: u64,
        raw: &[u8],
    ) -> io::Result<()> {
        let extracted = self.extract_fields(raw)?;
        let field_values = self.to_trace_format_values(&extracted);
        let mut values = Vec::with_capacity(1 + field_values.len());
        values.push(FieldValue::Varint(timestamp_ns));
        values.extend(field_values);
        encoder.write_event(schema, &values)
    }
}

/// A concrete value extracted from raw tracepoint data.
#[derive(Debug, Clone, PartialEq)]
pub enum RawFieldValue {
    U8(u8),
    U16(u16),
    U32(u32),
    U64(u64),
    I8(i8),
    I16(i16),
    I32(i32),
    I64(i64),
    /// Fixed-size string field (e.g. `char comm[16]`), NUL-trimmed.
    Str(String),
    /// Opaque bytes for unknown field types.
    Bytes(Vec<u8>),
}

// ---------------------------------------------------------------------------
// Format file parsing
// ---------------------------------------------------------------------------

/// Parse a single field line after the `field:` prefix.
///
/// Expected format: `<type> <name>;\toffset:<N>;\tsize:<N>;\tsigned:<0|1>;`
///
/// The type+name portion may contain array brackets like `char name[16]`.
fn parse_field_line(line: &str) -> Option<TracepointField> {
    // Split on `;` to get the parts.
    let parts: Vec<&str> = line.split(';').collect();
    if parts.len() < 4 {
        return None;
    }

    // First part: "<type> <name>" or "<type> <name>[N]"
    let type_and_name = parts[0].trim();

    // Parse offset, size, signed from remaining parts.
    let mut offset = None;
    let mut size = None;
    let mut signed = false;

    for part in &parts[1..] {
        let p = part.trim();
        if let Some(rest) = p.strip_prefix("offset:") {
            offset = rest.parse().ok();
        } else if let Some(rest) = p.strip_prefix("size:") {
            size = rest.parse().ok();
        } else if let Some(rest) = p.strip_prefix("signed:") {
            signed = rest.trim() == "1";
        }
    }

    let offset = offset?;
    let size = size?;

    // Detect __data_loc dynamic fields (e.g. "__data_loc char[] name").
    let is_dynamic = type_and_name.contains("__data_loc");

    // Split type and name. The name is the last whitespace-separated token,
    // but may have array brackets: `char name[16]` or `unsigned long addr`.
    let (field_type, name) = split_type_and_name(type_and_name)?;

    Some(TracepointField {
        name,
        field_type,
        offset,
        size,
        signed,
        is_dynamic,
    })
}

/// Split `"unsigned int nr"` into `("unsigned int", "nr")`,
/// `"char comm[16]"` into `("char[16]", "comm")`, or
/// `"__data_loc char[] name"` into `("__data_loc char[]", "name")`.
fn split_type_and_name(s: &str) -> Option<(String, String)> {
    let s = s.trim();

    if let Some(bracket_pos) = s.rfind('[') {
        let close = s[bracket_pos..].find(']').map(|i| bracket_pos + i)?;
        let after_bracket = s[close + 1..].trim();

        if after_bracket.is_empty() {
            // Brackets are on the name: "char prev_comm[16]"
            // → type = "char[16]", name = "prev_comm"
            let before_bracket = &s[..bracket_pos];
            let array_suffix = &s[bracket_pos..];
            let last_space = before_bracket.rfind(' ')?;
            let name = before_bracket[last_space + 1..].trim().to_string();
            let type_str = format!("{}{}", before_bracket[..last_space].trim(), array_suffix);
            Some((type_str, name))
        } else {
            // Brackets are on the type: "__data_loc char[] name"
            // → type = "__data_loc char[]", name = "name"
            let type_str = s[..close + 1].trim().to_string();
            let name = after_bracket.to_string();
            Some((type_str, name))
        }
    } else {
        // Simple: "unsigned int nr" → split on last space
        let last_space = s.rfind(' ')?;
        let field_type = s[..last_space].trim().to_string();
        let name = s[last_space + 1..].trim().to_string();
        Some((field_type, name))
    }
}

// ---------------------------------------------------------------------------
// Field value extraction
// ---------------------------------------------------------------------------

fn extract_field_value(bytes: &[u8], size: usize, signed: bool, field_type: &str) -> RawFieldValue {
    // String fields: char[N]
    if is_char_array(field_type) {
        let s = nul_terminated_str(bytes);
        return RawFieldValue::Str(s);
    }

    // TODO: from_ne_bytes assumes little-endian layout — not portable to big-endian.
    match (size, signed) {
        (1, false) => RawFieldValue::U8(bytes[0]),
        (1, true) => RawFieldValue::I8(bytes[0] as i8),
        (2, false) => RawFieldValue::U16(u16::from_ne_bytes(bytes[..2].try_into().unwrap())),
        (2, true) => RawFieldValue::I16(i16::from_ne_bytes(bytes[..2].try_into().unwrap())),
        (4, false) => RawFieldValue::U32(u32::from_ne_bytes(bytes[..4].try_into().unwrap())),
        (4, true) => RawFieldValue::I32(i32::from_ne_bytes(bytes[..4].try_into().unwrap())),
        (8, false) => RawFieldValue::U64(u64::from_ne_bytes(bytes[..8].try_into().unwrap())),
        (8, true) => RawFieldValue::I64(i64::from_ne_bytes(bytes[..8].try_into().unwrap())),
        _ => RawFieldValue::Bytes(bytes.to_vec()),
    }
}

fn is_char_array(field_type: &str) -> bool {
    // Matches "char[N]" patterns
    field_type.starts_with("char[") || field_type.starts_with("__kernel_char[")
}

/// Extract a NUL-terminated string from a fixed-size buffer.
fn nul_terminated_str(bytes: &[u8]) -> String {
    let end = bytes.iter().position(|&b| b == 0).unwrap_or(bytes.len());
    String::from_utf8_lossy(&bytes[..end]).into_owned()
}

// ---------------------------------------------------------------------------
// Kernel type → dial9-trace-format mapping
// ---------------------------------------------------------------------------

fn kernel_field_to_trace_format(field: &TracepointField) -> FieldDef {
    let field_type = if field.is_dynamic {
        FieldType::String
    } else {
        kernel_type_to_field_type(&field.field_type, field.size, field.signed)
    };
    FieldDef {
        name: field.name.clone(),
        field_type,
    }
}

fn kernel_type_to_field_type(type_str: &str, size: usize, signed: bool) -> FieldType {
    if is_char_array(type_str) {
        return FieldType::String;
    }

    // FieldValue doesn't have U8/U16/U32 variants (they decode as Varint),
    // so we map all unsigned integers to Varint for consistency.
    match (size, signed) {
        (1..=8, false) => FieldType::Varint,
        (1..=8, true) => FieldType::I64,
        _ => FieldType::Bytes,
    }
}

fn raw_to_trace_format_value(raw: &RawFieldValue) -> FieldValue {
    match raw {
        RawFieldValue::U8(v) => FieldValue::Varint(*v as u64),
        RawFieldValue::U16(v) => FieldValue::Varint(*v as u64),
        RawFieldValue::U32(v) => FieldValue::Varint(*v as u64),
        RawFieldValue::U64(v) => FieldValue::Varint(*v),
        RawFieldValue::I8(v) => FieldValue::I64(*v as i64),
        RawFieldValue::I16(v) => FieldValue::I64(*v as i64),
        RawFieldValue::I32(v) => FieldValue::I64(*v as i64),
        RawFieldValue::I64(v) => FieldValue::I64(*v),
        RawFieldValue::Str(s) => FieldValue::String(s.clone()),
        RawFieldValue::Bytes(b) => FieldValue::Bytes(b.clone()),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const SCHED_SWITCH_FORMAT: &str = r#"name: sched_switch
ID: 316
format:
	field:unsigned short common_type;	offset:0;	size:2;	signed:0;
	field:unsigned char common_flags;	offset:2;	size:1;	signed:0;
	field:unsigned char common_preempt_count;	offset:3;	size:1;	signed:0;
	field:int common_pid;	offset:4;	size:4;	signed:1;

	field:char prev_comm[16];	offset:8;	size:16;	signed:0;
	field:pid_t prev_pid;	offset:24;	size:4;	signed:1;
	field:int prev_prio;	offset:28;	size:4;	signed:1;
	field:long prev_state;	offset:32;	size:8;	signed:1;
	field:char next_comm[16];	offset:40;	size:16;	signed:0;
	field:pid_t next_pid;	offset:56;	size:4;	signed:1;
	field:int next_prio;	offset:60;	size:4;	signed:1;

print fmt: "prev_comm=%s prev_pid=%d prev_prio=%d prev_state=%s%s ==> next_comm=%s next_pid=%d next_prio=%d", REC->prev_comm, REC->prev_pid, REC->prev_prio, (REC->prev_state & ((((0x0000 | 0x0001 | 0x0002 | 0x0004 | 0x0008 | 0x0010 | 0x0020 | 0x0040) + 1) << 1) - 1)) ? __print_flags(REC->prev_state & ((((0x0000 | 0x0001 | 0x0002 | 0x0004 | 0x0008 | 0x0010 | 0x0020 | 0x0040) + 1) << 1) - 1), "|", { 0x0001, "S" }, { 0x0002, "D" }) : "R", REC->prev_state & (((0x0000 | 0x0001 | 0x0002 | 0x0004 | 0x0008 | 0x0010 | 0x0020 | 0x0040) + 1) << 1) ? "+" : "", REC->next_comm, REC->next_pid, REC->next_prio
"#;

    #[test]
    fn parse_sched_switch_format() {
        let tp = TracepointDef::parse_format(SCHED_SWITCH_FORMAT).unwrap();
        assert_eq!(tp.name, "sched_switch");
        assert_eq!(tp.id, 316);
        assert_eq!(tp.fields.len(), 7);

        // Check first field
        assert_eq!(tp.fields[0].name, "prev_comm");
        assert_eq!(tp.fields[0].field_type, "char[16]");
        assert_eq!(tp.fields[0].offset, 8);
        assert_eq!(tp.fields[0].size, 16);
        assert!(!tp.fields[0].signed);

        // Check prev_pid
        assert_eq!(tp.fields[1].name, "prev_pid");
        assert_eq!(tp.fields[1].offset, 24);
        assert_eq!(tp.fields[1].size, 4);
        assert!(tp.fields[1].signed);

        // Check prev_state (long, 8 bytes, signed)
        assert_eq!(tp.fields[3].name, "prev_state");
        assert_eq!(tp.fields[3].size, 8);
        assert!(tp.fields[3].signed);

        // Check next_prio (last field)
        assert_eq!(tp.fields[6].name, "next_prio");
        assert_eq!(tp.fields[6].offset, 60);
        assert_eq!(tp.fields[6].size, 4);
        assert!(tp.fields[6].signed);
    }

    #[test]
    fn common_fields_are_excluded() {
        let tp = TracepointDef::parse_format(SCHED_SWITCH_FORMAT).unwrap();
        for field in &tp.fields {
            assert!(
                !field.name.starts_with("common_"),
                "common field {} should be excluded",
                field.name
            );
        }
    }

    #[test]
    fn extract_fields_from_raw_data() {
        let tp = TracepointDef::parse_format(SCHED_SWITCH_FORMAT).unwrap();

        // Build fake raw data (64 bytes to cover all fields up to offset 60+4=64).
        let mut raw = vec![0u8; 64];

        // prev_comm at offset 8, size 16: "bash\0..."
        raw[8..12].copy_from_slice(b"bash");

        // prev_pid at offset 24, size 4, signed: 1234
        raw[24..28].copy_from_slice(&1234i32.to_ne_bytes());

        // prev_prio at offset 28, size 4, signed: 120
        raw[28..32].copy_from_slice(&120i32.to_ne_bytes());

        // prev_state at offset 32, size 8, signed: 1 (TASK_INTERRUPTIBLE)
        raw[32..40].copy_from_slice(&1i64.to_ne_bytes());

        // next_comm at offset 40, size 16: "tokio\0..."
        raw[40..45].copy_from_slice(b"tokio");

        // next_pid at offset 56, size 4, signed: 5678
        raw[56..60].copy_from_slice(&5678i32.to_ne_bytes());

        // next_prio at offset 60, size 4, signed: 120
        raw[60..64].copy_from_slice(&120i32.to_ne_bytes());

        let fields = tp.extract_fields(&raw).unwrap();
        assert_eq!(fields.len(), 7);

        assert_eq!(fields[0].0, "prev_comm");
        assert_eq!(fields[0].1, RawFieldValue::Str("bash".to_string()));

        assert_eq!(fields[1].0, "prev_pid");
        assert_eq!(fields[1].1, RawFieldValue::I32(1234));

        assert_eq!(fields[3].0, "prev_state");
        assert_eq!(fields[3].1, RawFieldValue::I64(1));

        assert_eq!(fields[4].0, "next_comm");
        assert_eq!(fields[4].1, RawFieldValue::Str("tokio".to_string()));

        assert_eq!(fields[5].0, "next_pid");
        assert_eq!(fields[5].1, RawFieldValue::I32(5678));
    }

    #[test]
    fn extract_fields_too_short_errors() {
        let tp = TracepointDef::parse_format(SCHED_SWITCH_FORMAT).unwrap();
        let raw = vec![0u8; 10]; // Way too short
        assert!(tp.extract_fields(&raw).is_err());
    }

    #[test]
    fn trace_format_field_mapping() {
        let tp = TracepointDef::parse_format(SCHED_SWITCH_FORMAT).unwrap();
        let defs = tp.to_trace_format_fields();
        assert_eq!(defs.len(), 7);

        // char[16] → String
        assert_eq!(defs[0].name, "prev_comm");
        assert_eq!(defs[0].field_type, FieldType::String);

        // pid_t (4 bytes, signed) → I64
        assert_eq!(defs[1].name, "prev_pid");
        assert_eq!(defs[1].field_type, FieldType::I64);

        // int (4 bytes, signed) → I64
        assert_eq!(defs[2].name, "prev_prio");
        assert_eq!(defs[2].field_type, FieldType::I64);

        // long (8 bytes, signed) → I64
        assert_eq!(defs[3].name, "prev_state");
        assert_eq!(defs[3].field_type, FieldType::I64);
    }

    #[test]
    fn trace_format_value_conversion() {
        let tp = TracepointDef::parse_format(SCHED_SWITCH_FORMAT).unwrap();

        let mut raw = vec![0u8; 64];
        raw[8..12].copy_from_slice(b"bash");
        raw[24..28].copy_from_slice(&42i32.to_ne_bytes());
        raw[28..32].copy_from_slice(&120i32.to_ne_bytes());
        raw[32..40].copy_from_slice(&1i64.to_ne_bytes());
        raw[40..45].copy_from_slice(b"tokio");
        raw[56..60].copy_from_slice(&99i32.to_ne_bytes());
        raw[60..64].copy_from_slice(&120i32.to_ne_bytes());

        let extracted = tp.extract_fields(&raw).unwrap();
        let values = tp.to_trace_format_values(&extracted);

        // prev_comm → String
        assert_eq!(values[0], FieldValue::String("bash".to_string()));

        // prev_pid → I64
        assert_eq!(values[1], FieldValue::I64(42));

        // prev_state → I64
        assert_eq!(values[3], FieldValue::I64(1));
    }

    #[test]
    fn trace_format_round_trip() {
        use dial9_trace_format::decoder::{DecodedFrame, Decoder};
        use dial9_trace_format::encoder::Encoder;

        let tp = TracepointDef::parse_format(SCHED_SWITCH_FORMAT).unwrap();

        // Register schema
        let mut encoder = Encoder::new();
        let schema = tp.register(&mut encoder).unwrap();

        // Build raw data and encode via encode_raw
        let mut raw = vec![0u8; 64];
        raw[8..12].copy_from_slice(b"bash");
        raw[24..28].copy_from_slice(&42i32.to_ne_bytes());
        raw[28..32].copy_from_slice(&120i32.to_ne_bytes());
        raw[32..40].copy_from_slice(&(-1i64).to_ne_bytes());
        raw[40..44].copy_from_slice(b"init");
        raw[56..60].copy_from_slice(&1i32.to_ne_bytes());
        raw[60..64].copy_from_slice(&120i32.to_ne_bytes());

        tp.encode_raw(&mut encoder, &schema, 5_000_000, &raw)
            .unwrap();

        // Decode and verify
        let bytes = encoder.finish();
        let mut decoder = Decoder::new(&bytes).unwrap();
        let frames = decoder.decode_all();

        let mut found_schema = false;
        let mut found_event = false;

        for frame in frames {
            match frame {
                DecodedFrame::Schema(entry) => {
                    assert_eq!(entry.name, "sched_switch");
                    assert!(entry.has_timestamp);
                    assert_eq!(entry.fields.len(), 7);
                    assert_eq!(entry.fields[0].name, "prev_comm");
                    found_schema = true;
                }
                DecodedFrame::Event {
                    timestamp_ns,
                    values,
                    ..
                } => {
                    assert_eq!(timestamp_ns, Some(5_000_000));
                    // prev_comm
                    assert_eq!(values[0], FieldValue::String("bash".to_string()));
                    // prev_pid
                    assert_eq!(values[1], FieldValue::I64(42));
                    // prev_state (negative)
                    assert_eq!(values[3], FieldValue::I64(-1));
                    // next_comm
                    assert_eq!(values[4], FieldValue::String("init".to_string()));
                    found_event = true;
                }
                _ => {}
            }
        }

        assert!(found_schema, "schema frame should be present");
        assert!(found_event, "event frame should be present");
    }

    // Minimal format for a simpler tracepoint
    const KMALLOC_FORMAT: &str = r#"name: kmalloc
ID: 404
format:
	field:unsigned short common_type;	offset:0;	size:2;	signed:0;
	field:unsigned char common_flags;	offset:2;	size:1;	signed:0;
	field:unsigned char common_preempt_count;	offset:3;	size:1;	signed:0;
	field:int common_pid;	offset:4;	size:4;	signed:1;

	field:unsigned long call_site;	offset:8;	size:8;	signed:0;
	field:const void * ptr;	offset:16;	size:8;	signed:0;
	field:size_t bytes_req;	offset:24;	size:8;	signed:0;
	field:size_t bytes_alloc;	offset:32;	size:8;	signed:0;
	field:unsigned int gfp_flags;	offset:40;	size:4;	signed:0;
	field:int node;	offset:44;	size:4;	signed:1;
"#;

    #[test]
    fn parse_kmalloc_format() {
        let tp = TracepointDef::parse_format(KMALLOC_FORMAT).unwrap();
        assert_eq!(tp.name, "kmalloc");
        assert_eq!(tp.id, 404);
        assert_eq!(tp.fields.len(), 6);

        assert_eq!(tp.fields[0].name, "call_site");
        assert_eq!(tp.fields[0].size, 8);
        assert!(!tp.fields[0].signed);

        // "const void *" is 8 bytes unsigned
        assert_eq!(tp.fields[1].name, "ptr");
        assert_eq!(tp.fields[1].size, 8);

        assert_eq!(tp.fields[4].name, "gfp_flags");
        assert_eq!(tp.fields[4].size, 4);
        assert!(!tp.fields[4].signed);

        assert_eq!(tp.fields[5].name, "node");
        assert!(tp.fields[5].signed);
    }

    #[test]
    fn type_mapping_unsigned_fields() {
        assert_eq!(
            kernel_type_to_field_type("unsigned char", 1, false),
            FieldType::Varint
        );
        assert_eq!(
            kernel_type_to_field_type("unsigned short", 2, false),
            FieldType::Varint
        );
        assert_eq!(
            kernel_type_to_field_type("unsigned int", 4, false),
            FieldType::Varint
        );
        assert_eq!(
            kernel_type_to_field_type("unsigned long", 8, false),
            FieldType::Varint
        );
        assert_eq!(
            kernel_type_to_field_type("size_t", 8, false),
            FieldType::Varint
        );
    }

    #[test]
    fn type_mapping_signed_fields() {
        assert_eq!(kernel_type_to_field_type("int", 4, true), FieldType::I64);
        assert_eq!(kernel_type_to_field_type("pid_t", 4, true), FieldType::I64);
        assert_eq!(kernel_type_to_field_type("long", 8, true), FieldType::I64);
    }

    #[test]
    fn type_mapping_char_array() {
        assert_eq!(
            kernel_type_to_field_type("char[16]", 16, false),
            FieldType::String
        );
    }

    #[test]
    fn split_type_name_simple() {
        let (ty, name) = split_type_and_name("unsigned int nr").unwrap();
        assert_eq!(ty, "unsigned int");
        assert_eq!(name, "nr");
    }

    #[test]
    fn split_type_name_array() {
        let (ty, name) = split_type_and_name("char prev_comm[16]").unwrap();
        assert_eq!(ty, "char[16]");
        assert_eq!(name, "prev_comm");
    }

    #[test]
    fn split_type_name_pointer() {
        let (ty, name) = split_type_and_name("const void * ptr").unwrap();
        assert_eq!(ty, "const void *");
        assert_eq!(name, "ptr");
    }

    const NETIF_RX_FORMAT: &str = r#"name: netif_rx
ID: 2691
format:
	field:unsigned short common_type;	offset:0;	size:2;	signed:0;
	field:unsigned char common_flags;	offset:2;	size:1;	signed:0;
	field:unsigned char common_preempt_count;	offset:3;	size:1;	signed:0;
	field:int common_pid;	offset:4;	size:4;	signed:1;

	field:void * skbaddr;	offset:8;	size:8;	signed:0;
	field:unsigned int len;	offset:16;	size:4;	signed:0;
	field:__data_loc char[] name;	offset:20;	size:4;	signed:0;
"#;

    #[test]
    fn parse_data_loc_field() {
        let tp = TracepointDef::parse_format(NETIF_RX_FORMAT).unwrap();
        assert_eq!(tp.fields.len(), 3);

        assert_eq!(tp.fields[2].name, "name");
        assert!(tp.fields[2].is_dynamic);
        assert_eq!(tp.fields[2].size, 4);

        // Non-dynamic fields should not be marked dynamic
        assert!(!tp.fields[0].is_dynamic);
        assert!(!tp.fields[1].is_dynamic);
    }

    #[test]
    fn extract_data_loc_field() {
        let tp = TracepointDef::parse_format(NETIF_RX_FORMAT).unwrap();

        // Build raw data: fixed part (24 bytes) + dynamic string appended after
        let mut raw = vec![0u8; 32];

        // skbaddr at offset 8, size 8
        raw[8..16].copy_from_slice(&0xdeadbeef_u64.to_ne_bytes());
        // len at offset 16, size 4
        raw[16..20].copy_from_slice(&100_u32.to_ne_bytes());
        // __data_loc at offset 20: string starts at byte 24, length 5 ("eth0\0")
        let loc: u32 = (24 << 16) | 5;
        raw[20..24].copy_from_slice(&loc.to_ne_bytes());
        // Dynamic data at offset 24
        raw[24..28].copy_from_slice(b"eth0");
        raw[28] = 0; // NUL terminator

        let fields = tp.extract_fields(&raw).unwrap();
        assert_eq!(fields[2].0, "name");
        assert_eq!(fields[2].1, RawFieldValue::Str("eth0".to_string()));
    }

    #[test]
    fn data_loc_trace_format_mapping() {
        let tp = TracepointDef::parse_format(NETIF_RX_FORMAT).unwrap();
        let defs = tp.to_trace_format_fields();

        // __data_loc char[] → String
        assert_eq!(defs[2].name, "name");
        assert_eq!(defs[2].field_type, FieldType::String);
    }
}
