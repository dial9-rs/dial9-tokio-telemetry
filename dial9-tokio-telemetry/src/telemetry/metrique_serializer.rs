//! Binary serialization for metrique entries.
//!
//! ## `Operation` property convention
//!
//! Metrique entries should include a string property named `Operation` (case-insensitive
//! match). This property identifies the logical operation the entry represents
//! (e.g. `"GetItem"`, `"PutRecord"`). The trace viewer uses the `Operation` value as the
//! display name for span bars and grouping — events without an `Operation` property are
//! ignored by the viewer's span chart and grouping logic, though they are still recorded
//! in the trace for use by other analysis tooling.

#[cfg(feature = "metrique-events")]
use metrique::writer::{EntryWriter, Observation, Unit, ValueWriter};
#[cfg(feature = "metrique-events")]
use std::borrow::Cow;

use serde::ser::{SerializeMap, Serializer};
use serde::Serialize;

/// EntryWriter that writes directly into a byte buffer without intermediate allocations.
///
/// Layout (identical to the previous format):
/// ```text
/// [num_properties: u16]
///   [key_len: u16][key: N][value_len: u16][value: N] ...
/// [num_metrics: u16]
///   [name_len: u16][name: N][flags: u8][unit_len: u8][unit: N][obs_count: u16][values: 8*count] ...
/// ```
///
/// Counts are written as zero placeholders, then patched in-place once all items are written.
#[cfg(feature = "metrique-events")]
pub struct BinaryEntryWriter {
    buf: Vec<u8>,
    /// Offset of the property count u16 in `buf`.
    prop_count_offset: usize,
    prop_count: u16,
    /// Offset of the metric count u16 (written when first metric arrives or at finalize).
    metric_count_offset: Option<usize>,
    metric_count: u16,
    /// True once we've transitioned from properties to metrics (or finalized).
    in_metrics: bool,
}

#[cfg(feature = "metrique-events")]
impl BinaryEntryWriter {
    pub fn new() -> Self {
        let mut buf = Vec::with_capacity(256);
        // Reserve space for property count
        buf.extend_from_slice(&0u16.to_le_bytes());
        Self {
            buf,
            prop_count_offset: 0,
            prop_count: 0,
            metric_count_offset: None,
            metric_count: 0,
            in_metrics: false,
        }
    }

    /// Transition to the metrics section if not already there.
    fn begin_metrics(&mut self) {
        if !self.in_metrics {
            self.in_metrics = true;
            // Patch property count
            let bytes = self.prop_count.to_le_bytes();
            self.buf[self.prop_count_offset] = bytes[0];
            self.buf[self.prop_count_offset + 1] = bytes[1];
            // Write placeholder for metric count
            self.metric_count_offset = Some(self.buf.len());
            self.buf.extend_from_slice(&0u16.to_le_bytes());
        }
    }

    fn write_property(&mut self, key: &str, value: &str) {
        self.prop_count += 1;
        self.buf.extend_from_slice(&(key.len() as u16).to_le_bytes());
        self.buf.extend_from_slice(key.as_bytes());
        self.buf.extend_from_slice(&(value.len() as u16).to_le_bytes());
        self.buf.extend_from_slice(value.as_bytes());
    }

    pub fn into_bytes(mut self) -> Vec<u8> {
        // Ensure metrics section is started (writes prop count patch + metric count placeholder)
        self.begin_metrics();
        // Patch metric count
        if let Some(off) = self.metric_count_offset {
            let bytes = self.metric_count.to_le_bytes();
            self.buf[off] = bytes[0];
            self.buf[off + 1] = bytes[1];
        }
        self.buf
    }
}

#[cfg(feature = "metrique-events")]
impl<'a> EntryWriter<'a> for BinaryEntryWriter {
    fn timestamp(&mut self, _timestamp: std::time::SystemTime) {}

    fn value(
        &mut self,
        name: impl Into<Cow<'a, str>>,
        value: &(impl metrique::writer::Value + ?Sized),
    ) {
        let name = name.into();
        let collector = DirectValueWriter {
            name: &name,
            writer: self,
        };
        value.write(collector);
    }

    fn config(&mut self, _config: &'a dyn metrique::writer::EntryConfig) {}
}

#[cfg(feature = "metrique-events")]
struct DirectValueWriter<'w, 'a> {
    name: &'w Cow<'a, str>,
    writer: &'w mut BinaryEntryWriter,
}

#[cfg(feature = "metrique-events")]
impl ValueWriter for DirectValueWriter<'_, '_> {
    fn string(self, value: &str) {
        // Properties must come before metrics
        debug_assert!(!self.writer.in_metrics, "string property after metric");
        self.writer.write_property(self.name, value);
    }

    fn metric<'a>(
        self,
        distribution: impl IntoIterator<Item = Observation>,
        unit: Unit,
        _dimensions: impl IntoIterator<Item = (&'a str, &'a str)>,
        flags: metrique::writer::MetricFlags<'_>,
    ) {
        self.writer.begin_metrics();
        self.writer.metric_count += 1;

        let buf = &mut self.writer.buf;
        // Name
        let name = self.name.as_ref();
        buf.extend_from_slice(&(name.len() as u16).to_le_bytes());
        buf.extend_from_slice(name.as_bytes());
        // Flags
        let is_span = flags
            .downcast::<crate::telemetry::entry_sink::SpanFlag>()
            .is_some();
        buf.push(if is_span { 1u8 } else { 0u8 });
        // Unit
        let unit_name = unit.name();
        buf.push(unit_name.len() as u8);
        buf.extend_from_slice(unit_name.as_bytes());
        // Observation count placeholder
        let count_offset = buf.len();
        buf.extend_from_slice(&0u16.to_le_bytes());
        let mut count: u16 = 0;
        for obs in distribution {
            let val = match obs {
                Observation::Floating(v) => v,
                Observation::Unsigned(v) => v as f64,
                Observation::Repeated { total, .. } => total,
                _ => f64::NAN,
            };
            buf.extend_from_slice(&val.to_le_bytes());
            count += 1;
        }
        // Patch observation count
        let bytes = count.to_le_bytes();
        buf[count_offset] = bytes[0];
        buf[count_offset + 1] = bytes[1];
    }

    fn error(self, _error: metrique::writer::ValidationError) {}
}

#[cfg(feature = "metrique-events")]
pub fn serialize_entry<E: metrique::writer::Entry>(entry: &E) -> Vec<u8> {
    let mut writer = BinaryEntryWriter::new();
    entry.write(&mut writer);
    writer.into_bytes()
}

/// Parsed representation of the binary metrique data blob.
#[derive(Debug, Clone, Serialize)]
pub struct ParsedMetriqueData {
    pub properties: Vec<(String, String)>,
    pub metrics: Vec<ParsedMetric>,
}

#[derive(Debug, Clone, Serialize)]
pub struct ParsedMetric {
    pub name: String,
    pub is_span: bool,
    pub unit: String,
    pub values: Vec<f64>,
}

/// Parse the binary data blob written by `BinaryEntryWriter::into_bytes`.
pub fn parse_metrique_data(data: &[u8]) -> Option<ParsedMetriqueData> {
    let mut off = 0;
    let r = |off: &mut usize, n: usize| -> Option<&[u8]> {
        if *off + n > data.len() { return None; }
        let s = &data[*off..*off + n];
        *off += n;
        Some(s)
    };

    let num_props = u16::from_le_bytes(r(&mut off, 2)?.try_into().ok()?) as usize;
    let mut properties = Vec::with_capacity(num_props);
    for _ in 0..num_props {
        let kl = u16::from_le_bytes(r(&mut off, 2)?.try_into().ok()?) as usize;
        let key = std::str::from_utf8(r(&mut off, kl)?).ok()?.to_string();
        let vl = u16::from_le_bytes(r(&mut off, 2)?.try_into().ok()?) as usize;
        let val = std::str::from_utf8(r(&mut off, vl)?).ok()?.to_string();
        properties.push((key, val));
    }

    let num_metrics = u16::from_le_bytes(r(&mut off, 2)?.try_into().ok()?) as usize;
    let mut metrics = Vec::with_capacity(num_metrics);
    for _ in 0..num_metrics {
        let nl = u16::from_le_bytes(r(&mut off, 2)?.try_into().ok()?) as usize;
        let name = std::str::from_utf8(r(&mut off, nl)?).ok()?.to_string();
        let flags = r(&mut off, 1)?[0];
        let unit_len = r(&mut off, 1)?[0] as usize;
        let unit = std::str::from_utf8(r(&mut off, unit_len)?).ok()?.to_string();
        let count = u16::from_le_bytes(r(&mut off, 2)?.try_into().ok()?) as usize;
        let mut values = Vec::with_capacity(count);
        for _ in 0..count {
            values.push(f64::from_le_bytes(r(&mut off, 8)?.try_into().ok()?));
        }
        metrics.push(ParsedMetric { name, is_span: flags & 1 != 0, unit, values });
    }

    Some(ParsedMetriqueData { properties, metrics })
}

/// Custom serde serializer for the `data` field that emits parsed structure instead of raw bytes.
pub fn serialize_metrique_data<S: Serializer>(data: &Vec<u8>, s: S) -> Result<S::Ok, S::Error> {
    match parse_metrique_data(data) {
        Some(parsed) => parsed.serialize(s),
        None => {
            // Fallback: emit as map with raw bytes
            let mut m = s.serialize_map(Some(1))?;
            m.serialize_entry("raw", data)?;
            m.end()
        }
    }
}
