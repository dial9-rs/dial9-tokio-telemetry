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

#### 1. Macro-based Sink Definition

```rust
dial9_sink! { RequestMetrics<MyRequestEntry> }
```

Generates:
- `RequestMetrics` struct implementing `EntrySink<MyRequestEntry>`
- Static `INSTANCE` with type-safe access (using `OnceLock`)
- `RequestMetrics::attach_backing_sink(sink)` method

The macro ensures strong typing and avoids boxing until the backing sink.

#### 2. Force Flags

Define dial9-specific flags following metrique's `ForceFlag` pattern:

```rust
pub struct Kpi;
impl ForceFlag for Kpi { /* ... */ }

pub struct Dial9Metadata<T> {
    start: Timestamp,
    end: TimestampOnClose,
    _phantom: PhantomData<T>,
}
```

Usage:
```rust
#[metrics]
struct MyRequestEntry {
    dial9: Dial9Metadata<MetriqueEvent>,
    #[metric(flags = [Kpi])]
    latency_ms: f64,
    // ...
}
```

#### 3. Serialization

Two-pass serialization for efficiency:

```rust
struct BinaryValueWriter {
    buf: Vec<u8>,
    mode: WriteMode, // SizeCalculation or Writing
}

impl ValueWriter for BinaryValueWriter {
    // First pass: compute required size
    // Second pass: write to pre-allocated buffer
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
  [key_len:2][key:N][flags:1][count:2][values:8*count] ... (for each metric)

flags byte:
  bit 0: is_kpi
  bits 1-7: reserved
```

Wire tag: `WIRE_METRIQUE_EVENT = 0x0A`

#### 5. Event Types

```rust
// RawEvent (in-memory, pre-interning)
#[cfg(feature = "metrique-events")]
RawEvent::MetriqueEvent {
    timestamp_nanos: u64,
    worker_id: usize,
    data: Vec<u8>,  // Serialized entry
}

// TelemetryEvent (wire format, post-interning)
#[cfg(feature = "metrique-events")]
TelemetryEvent::MetriqueEvent {
    timestamp_nanos: u64,
    worker_id: usize,
    entry_name: String,
    kpi_field_names: Vec<String>,  // For viewer to graph
    data: Vec<u8>,
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

### Phase 2: Flags and Metadata
- [ ] Define `Kpi` force flag
- [ ] Define `Dial9Metadata<T>` with `Timestamp` and `TimestampOnClose`
- [ ] Implement flag serialization in `BinaryValueWriter`

### Phase 3: Macro and Sink
- [ ] Implement `dial9_sink!` macro
- [ ] Generate strongly-typed sink struct
- [ ] Generate static instance with `OnceLock`
- [ ] Implement `attach_backing_sink` method
- [ ] Implement `EntrySink` trait for generated sink

### Phase 4: Wire Format
- [ ] Implement `write_event` for `TelemetryEvent::MetriqueEvent`
- [ ] Implement `read_event` for `TelemetryEvent::MetriqueEvent`
- [ ] Add tests for serialization roundtrip

### Phase 5: Viewer Support
- [ ] Update trace viewer to parse metrique events
- [ ] Add KPI graphing UI
- [ ] Add metrique event filtering/search

## Design Decisions

### Why not intern field names?
Metrique events are less frequent than poll events, and the overhead of storing field names inline is acceptable for v0. We can add interning later if needed.

### Why 2-pass serialization?
Pre-allocating the exact buffer size avoids reallocations and is more efficient than growing the buffer dynamically.

### Why store all observations?
Most metrique users will use compressed histograms (which are already compact). Storing all observations gives maximum fidelity for analysis.

### Why macro instead of generic function?
The macro generates a static with strong typing, avoiding the need for `BoxEntrySink` at the call site. This keeps the hot path zero-cost.

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
