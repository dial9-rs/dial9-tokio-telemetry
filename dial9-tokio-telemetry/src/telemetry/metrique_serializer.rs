//! Binary serialization for metrique entries.

#[cfg(feature = "metrique-events")]
use metrique::writer::{EntryWriter, Observation, Unit, ValueWriter};
#[cfg(feature = "metrique-events")]
use std::borrow::Cow;

use serde::ser::{SerializeMap, Serializer};
use serde::Serialize;

/// Collects entry data for serialization
#[cfg(feature = "metrique-events")]
#[derive(Debug)]
struct CollectedEntry {
    properties: Vec<(String, String)>,
    metrics: Vec<CollectedMetric>,
}

#[cfg(feature = "metrique-events")]
#[derive(Debug)]
struct CollectedMetric {
    name: String,
    observations: Vec<Observation>,
    unit: Unit,
    is_span: bool,
}

/// EntryWriter that collects all fields
#[cfg(feature = "metrique-events")]
pub struct BinaryEntryWriter {
    entry: CollectedEntry,
}

#[cfg(feature = "metrique-events")]
impl BinaryEntryWriter {
    pub fn new() -> Self {
        Self {
            entry: CollectedEntry {
                properties: Vec::new(),
                metrics: Vec::new(),
            },
        }
    }

    pub fn into_bytes(self) -> Vec<u8> {
        let mut size = 0;
        
        // Calculate size
        size += 2; // property count
        for (key, value) in &self.entry.properties {
            size += 2 + key.len() + 2 + value.len();
        }
        
        size += 2; // metric count
        for metric in &self.entry.metrics {
            size += 2 + metric.name.len(); // name
            size += 1; // flags
            size += 1 + metric.unit.name().len(); // unit name (u8 len + bytes)
            size += 2; // observation count
            size += metric.observations.len() * 8; // observations as f64
        }
        
        // Allocate and write
        let mut buf = Vec::with_capacity(size);
        
        // Write properties
        buf.extend_from_slice(&(self.entry.properties.len() as u16).to_le_bytes());
        for (key, value) in &self.entry.properties {
            buf.extend_from_slice(&(key.len() as u16).to_le_bytes());
            buf.extend_from_slice(key.as_bytes());
            buf.extend_from_slice(&(value.len() as u16).to_le_bytes());
            buf.extend_from_slice(value.as_bytes());
        }
        
        // Write metrics
        buf.extend_from_slice(&(self.entry.metrics.len() as u16).to_le_bytes());
        for metric in &self.entry.metrics {
            buf.extend_from_slice(&(metric.name.len() as u16).to_le_bytes());
            buf.extend_from_slice(metric.name.as_bytes());
            
            let flags = if metric.is_span { 1u8 } else { 0u8 };
            buf.push(flags);
            
            let unit_name = metric.unit.name();
            buf.push(unit_name.len() as u8);
            buf.extend_from_slice(unit_name.as_bytes());
            
            buf.extend_from_slice(&(metric.observations.len() as u16).to_le_bytes());
            for obs in &metric.observations {
                let val = match obs {
                    Observation::Floating(v) => *v,
                    Observation::Unsigned(v) => *v as f64,
                    Observation::Repeated { total, .. } => *total,
                    _ => f64::NAN,
                };
                buf.extend_from_slice(&val.to_le_bytes());
            }
        }
        
        buf
    }
}

#[cfg(feature = "metrique-events")]
impl<'a> EntryWriter<'a> for BinaryEntryWriter {
    fn timestamp(&mut self, _timestamp: std::time::SystemTime) {
        // Timestamp is handled separately at the event level
    }

    fn value(
        &mut self,
        name: impl Into<Cow<'a, str>>,
        value: &(impl metrique::writer::Value + ?Sized),
    ) {
        let name = name.into().to_string();
        let collector = ValueCollector {
            name,
            entry: &mut self.entry,
        };
        value.write(collector);
    }

    fn config(&mut self, _config: &'a dyn metrique::writer::EntryConfig) {
        // No config needed for binary serialization
    }
}

#[cfg(feature = "metrique-events")]
struct ValueCollector<'a> {
    name: String,
    entry: &'a mut CollectedEntry,
}

#[cfg(feature = "metrique-events")]
impl ValueWriter for ValueCollector<'_> {
    fn string(self, value: &str) {
        self.entry.properties.push((self.name, value.to_string()));
    }

    fn metric<'a>(
        self,
        distribution: impl IntoIterator<Item = Observation>,
        unit: Unit,
        _dimensions: impl IntoIterator<Item = (&'a str, &'a str)>,
        flags: metrique::writer::MetricFlags<'_>,
    ) {
        let observations: Vec<_> = distribution.into_iter().collect();
        let is_span = flags
            .downcast::<crate::telemetry::entry_sink::SpanFlag>()
            .is_some();
        self.entry.metrics.push(CollectedMetric {
            name: self.name,
            observations,
            unit,
            is_span,
        });
    }

    fn error(self, _error: metrique::writer::ValidationError) {
        // Ignore errors for now
    }
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
