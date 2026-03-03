//! Binary serialization for metrique entries.

#[cfg(feature = "metrique-events")]
use metrique::writer::{EntryWriter, Observation, Unit, ValueWriter};
#[cfg(feature = "metrique-events")]
use std::borrow::Cow;

#[cfg(feature = "metrique-events")]
#[derive(Debug)]
enum WriteMode {
    SizeCalculation,
    Writing,
}

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
    is_kpi: bool,
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
            
            let flags = if metric.is_kpi { 1u8 } else { 0u8 };
            buf.push(flags);
            
            buf.extend_from_slice(&(metric.observations.len() as u16).to_le_bytes());
            for obs in &metric.observations {
                let val = match obs {
                    Observation::Floating(v) => *v,
                    Observation::Unsigned(v) => *v as f64,
                    Observation::Repeated { total, occurrences } => total / *occurrences as f64,
                    _ => 0.0, // Handle other variants
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
        let mut collector = ValueCollector {
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
        _unit: Unit,
        _dimensions: impl IntoIterator<Item = (&'a str, &'a str)>,
        _flags: metrique::writer::MetricFlags<'_>,
    ) {
        let observations: Vec<_> = distribution.into_iter().collect();
        // TODO: check flags for KPI marker
        self.entry.metrics.push(CollectedMetric {
            name: self.name,
            observations,
            unit: _unit,
            is_kpi: false, // TODO: extract from flags
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
