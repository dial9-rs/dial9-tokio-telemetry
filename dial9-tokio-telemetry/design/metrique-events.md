# Metrique Events Design

## Overview

Add support for capturing metrique entries in dial9 traces. Metrique events are flat maps of key-value pairs where values can be properties (strings) or metrics (numeric data with multiple observations).

## Goals

1. Capture metrique entries in dial9 traces with minimal overhead
2. Allow marking specific fields as KPIs for graphing in the viewer
3. Dual-sink architecture: events go to both dial9 trace and customer's logging sink
4. Type-safe, zero-cost abstraction using macros

## Architecture

### Dual-Sink Flow

```
metrique entry → Dial9MetricSink<E> → [serialize to RawEvent] → thread-local buffer
                                     ↓
                                     BoxEntrySink<E> (customer's logging)
```

### Components

#### 1. Entry Sink

```rust
use dial9_tokio_telemetry::telemetry::{Dial9EntrySink, Kpi};

static SINK: Dial9EntrySink<metrique::RootMetric<RequestEntry>> =
    Dial9EntrySink::new("RequestEntry");
```

`Dial9EntrySink<E>` implements `EntrySink<E>` and writes serialized entries
into the thread-local trace buffer. It's `const fn new` so it works as a static.

#### 2. KPI Flag

`Kpi<T>` is a `ForceFlag` type alias that marks a metric field for graphing in the viewer:

```rust
pub type Kpi<T> = ForceFlag<T, KpiFlagCtor>;
```

The flag is detected during serialization via `flags.downcast::<KpiFlag>()` and
written as bit 0 of the per-metric flags byte in the data blob.

Usage:
```rust
#[metrics]
struct MyRequestEntry {
    #[metrics(unit = Millisecond)]
    latency: Kpi<Timer>,
    // ...
}
```

#### 3. Serialization

`BinaryEntryWriter` implements metrique's `EntryWriter` trait, collecting properties
and metrics. `into_bytes()` pre-calculates the exact size and writes in one pass:

```rust
pub fn serialize_entry<E: Entry>(entry: &E) -> Vec<u8> {
    let mut writer = BinaryEntryWriter::new();
    entry.write(&mut writer);
    writer.into_bytes()
}
```

#### 4. Wire Format

```
MetriqueEvent wire format:
[tag:1][timestamp_us:4][worker:1][entry_name_len:2][entry_name:N][data_len:4][data:N]

data format:
[num_properties:2]
  [key_len:2][key:N][value_len:2][value:N] ... (for each property)
[num_metrics:2]
  [key_len:2][key:N][flags:1][unit_len:1][unit:N][count:2][values:8*count] ... (for each metric)

flags byte:
  bit 0: is_kpi
  bits 1-7: reserved
```

Wire tag: `WIRE_METRIQUE_EVENT = 11`

#### 5. Event Types

```rust
// RawEvent (in-memory, pre-interning)
#[cfg(feature = "metrique-events")]
RawEvent::MetriqueEvent {
    timestamp_nanos: u64,
    worker_id: usize,
    entry_name: String,
    data: Vec<u8>,  // Serialized entry
}

// TelemetryEvent (wire format, post-interning)
#[cfg(feature = "metrique-events")]
TelemetryEvent::MetriqueEvent {
    timestamp_nanos: u64,
    worker_id: usize,
    entry_name: String,
    data: Vec<u8>,  // KPI info is in per-metric flags byte
}
```

#### 6. Buffer Integration

Add to `ThreadLocalBuffer`:

```rust
#[cfg(feature = "metrique-events")]
pub fn record_metrique_entry(&mut self, entry: &impl Entry) {
    let data = serialize_entry(entry);
    self.record_event(RawEvent::MetriqueEvent {
        timestamp_nanos: entry.timestamp().unwrap_or_else(now),
        worker_id: current_worker_id(),
        data,
    });
}
```

## Implementation Checklist

### Phase 1: Core Infrastructure ✅
- [x] Add `metrique` dependency with `metrique-events` feature
- [x] Define `RawEvent::MetriqueEvent` and `TelemetryEvent::MetriqueEvent`
- [x] Define wire format constant `WIRE_METRIQUE_EVENT`
- [x] Implement `BinaryEntryWriter` and `BinaryValueWriter` for serialization
- [x] Add `record_metrique_entry` to `ThreadLocalBuffer`
- [x] Add match arms for MetriqueEvent in all event handling code

### Phase 2: Flags and Metadata ✅
- [x] Define `Kpi` force flag (via `ForceFlag<T, KpiFlagCtor>`)
- [x] Implement flag serialization in `BinaryEntryWriter` (flags.downcast::<KpiFlag>() → bit 0)
- [ ] Define `Dial9Metadata<T>` with `Timestamp` and `TimestampOnClose` (deferred — not needed for v0)

### Phase 3: Sink ✅
- [x] Implement `Dial9EntrySink<E>` struct with `EntrySink` trait
- [x] Static instance support (const fn new, works with `static SINK: Dial9EntrySink<...>`)
- [ ] Implement `dial9_sink!` macro (deferred — manual sink works fine)
- [ ] Implement `attach_backing_sink` for dual-sink (deferred)

### Phase 4: Wire Format ✅
- [x] Implement `write_event` for `TelemetryEvent::MetriqueEvent`
- [x] Implement `read_event` for `TelemetryEvent::MetriqueEvent`
- [x] Skip metrique events gracefully when feature is disabled
- [x] Add tests for serialization roundtrip
- [x] Unit name serialized in data blob (u8 len-prefixed)

### Phase 5: Example Integration ✅
- [x] Wire up metrique in metrics-service example
- [x] Define `RequestEntry` with `Kpi<Timer>` for duration

### Phase 6: Viewer Support
- [ ] Update trace viewer JS parser to handle wire code 11
- [ ] Parse data blob (properties, metrics with flags/unit)
- [ ] Render metrique events in the UI
- [ ] KPI graphing

## Design Decisions

### Why not intern field names?
Metrique events are less frequent than poll events, and the overhead of storing field names inline is acceptable for v0. We can add interning later if needed.

### Why store all observations?
Most metrique users will use compressed histograms (which are already compact). Storing all observations gives maximum fidelity for analysis.

### Why KPI in the data blob instead of the wire envelope?
Each metric already has a flags byte in the data blob. Storing KPI as bit 0 there costs zero extra bytes, vs duplicating field names in a separate `kpi_field_names` list in the wire format.

### Why `Dial9EntrySink` instead of a macro?
A simple generic struct with `const fn new` covers the common case (static sink, single entry type). A macro could be added later for dual-sink (trace + logging) support.

## Open Questions

1. Should we support metrique's namespace concept if it exists?
2. How should we handle very large entries (e.g., >1MB)? Truncate? Skip?
3. Should we batch metrique events separately from other events?
4. Do we need a way to disable metrique capture at runtime without removing the sink?

## Future Enhancements

- Field name interning for space efficiency
- Parquet output for metrique events
- Aggregation/downsampling for high-frequency metrics
- Integration with metrique's existing EMF/CloudWatch sinks
