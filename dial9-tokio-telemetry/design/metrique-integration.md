# Metrique integration

> **Status: design, not yet implemented.**

Dial9 is a peer metrique sink. Users configure dial9 alongside their existing EMF/JSON metrique pipeline; every metrique entry that flows through the configured sink is also recorded into the dial9 trace. A single trace file carries both tokio runtime telemetry and per-request application metrics.

The sink reads metrique's entry descriptor for each entry to learn its structural shape (fields, optionality, `Flex`, units), identifies caller-thread context via a sink-specific field tag on flattened context fields, and encodes the user-selected subset of fields into the dial9 trace. Nothing about the integration requires a dial9-specific metrique macro or dial9-specific newtype wrappers on fields.

This design depends on the entry descriptor system in metrique (see `docs/entry-descriptors.md` in the metrique repo; tracked under [awslabs/metrique#282](https://github.com/awslabs/metrique/pull/282)). The dial9 side is a descriptor-aware sink; the metrique side is where descriptors and field tags are defined.

## User-facing API

### Opt-in on the entry

```rust
use dial9::{Dial9Context, Emit, Interned};

#[metrics(default_field_tag(Emit))]
struct RequestMetrics {
    // Dial9 context fields. Flatten with skip(Emit) so context data is not
    // duplicated into the dial9 payload; the sink picks it up via the
    // dial9::Context tag that Dial9Context's fields carry.
    #[metrics(flatten, field_tag(skip(Emit)))]
    dial9: Dial9Context,

    #[metrics(field_tag(Interned))]
    route: String,

    operation: &'static str,
    request_id: String,

    #[metrics(field_tag(skip(Emit)))]
    debug_blob: String,
}
```

What this means:

- `Dial9Context` is a dial9-provided metrique struct. Its fields (worker id, task id, start monotonic timestamp, end monotonic timestamp) are tagged with a dial9-internal `dial9::Context` marker. The constructor captures caller-thread start-time state; the end-time monotonic is captured via `CloseValue` at close.
- `flatten` spreads `Dial9Context`'s fields into the parent. `field_tag(skip(Emit))` on the flatten site propagates to the flattened children as their default, so the context fields aren't duplicated into the dial9 payload.
- `Emit` marks fields that should appear in the dial9 trace payload. `skip(Emit)` at the field level overrides.
- `Interned` tells the sink to route string data in this field through dial9's string pool.

### Sink construction

```rust
use dial9::AttachDial9Ext;
use metrique::ServiceMetrics;

let _handle = ServiceMetrics::attach_to_stream_with_dial9(
    emf_stream,
    &telemetry_handle,
);
```

The builder and manual composition paths are unchanged from the original design. `metrique_sink(emf_stream, &telemetry_handle).build()` returns a standalone sink; `tee(emf_stream, Dial9Stream::new(&telemetry_handle))` is the primitive composition for users who want to wire their own.

## Architecture

```text
┌────────────────────────────────────────────────────────────────┐
│ COMPILE TIME: metrique macro                                   │
│                                                                │
│ Dial9 defines (in its own crate):                              │
│   pub struct Emit;          // field tag for payload           │
│   pub struct Interned;      // field tag for string pool       │
│   #[doc(hidden)] pub struct Context;  // internal marker       │
│                                                                │
│   #[metrics]                                                   │
│   pub struct Dial9Context { /* fields tagged Context */ }      │
│                                                                │
│ User-side:                                                     │
│   #[metrics(default_field_tag(Emit))]                          │
│   struct RequestMetrics {                                      │
│       #[metrics(flatten, field_tag(skip(Emit)))]               │
│       dial9: Dial9Context,                                     │
│                                                                │
│       #[metrics(field_tag(Interned))]                          │
│       route: String,                                           │
│       ...                                                      │
│   }                                                            │
│                                                                │
│ Macro emits:                                                   │
│   impl Entry for ClosedRequestMetrics (as today)               │
│   static EntryDescriptor (fields, tags, units, canonical name) │
│   impl Entry::descriptor() returning Some(DescriptorRef)       │
└────────────────────────────────────────────────────────────────┘
                              │
                              ▼
┌────────────────────────────────────────────────────────────────┐
│ CALLER THREAD: request path                                    │
│                                                                │
│ let m = RequestMetrics { dial9: Dial9Context::capture(), ... };│
│   Dial9Context::capture() reads:                               │
│     tokio worker id, task id, monotonic clock (start)          │
│   other fields populated normally                              │
│                                                                │
│ Caller-thread overhead: a few TL reads + clock_monotonic_ns()  │
│ per entry. No allocations beyond what metrique already does.   │
└────────────────────────────────────────────────────────────────┘
                              │
                              ▼
┌────────────────────────────────────────────────────────────────┐
│ CALLER THREAD: append-on-drop / close                          │
│                                                                │
│ All CloseValue runs (Timer, Duration, Option, ...).            │
│ Dial9Context's CloseValue reads the monotonic clock (end).     │
│                                                                │
│ Entry is pushed to BackgroundQueue as BoxEntry.                │
└────────────────────────────────────────────────────────────────┘
                              │
                              ▼
┌────────────────────────────────────────────────────────────────┐
│ FLUSH THREAD: BackgroundQueue / tee                            │
│                                                                │
│ Each entry is delivered to every registered sink:              │
│                                                                │
│   ├── EMF sink: calls Entry::write as today.                   │
│   │             Does not call descriptor().                    │
│   │                                                            │
│   └── Dial9Stream (descriptor-aware):                          │
│         desc = entry.descriptor()                              │
│           None    -> skip (hand-written entry, report once)    │
│           Some(d) -> continue                                  │
│                                                                │
│         on first-use per DescriptorId, compute:                │
│           context_fields: indices into d.fields() where the    │
│                           dial9::Context tag is present        │
│           payload_fields: indices where Emit is present        │
│                                                                │
│         cache those indices keyed on d.id()                    │
└────────────────────────────────────────────────────────────────┘
                              │
                              ▼
┌────────────────────────────────────────────────────────────────┐
│ FLUSH THREAD: inside Dial9Stream, per entry                    │
│                                                                │
│ Walk entry.write(Dial9EntryWriter { ... }):                    │
│   for each (name, value) callback (in descriptor order):       │
│     if index is in context_fields:                             │
│       pull value into the trace event header (worker, task,    │
│       monotonic_ns_start, monotonic_ns_end)                    │
│                                                                │
│     else if index is in payload_fields:                        │
│       encode according to FieldShape:                          │
│            Known   : encode scalar                             │
│            Optional: encode presence byte + inner              │
│            List    : encode <count> <repeated element>         │
│            Flex    : encode map<key, value>                    │
│            Opaque  : report + skip (sink-side validation)      │
│                                                                │
│     if field is tagged Interned and carries string data:       │
│       route through encoder.intern_string(..)                  │
│                                                                │
│ encoder.finish_event()                                         │
└────────────────────────────────────────────────────────────────┘
```

Work on the caller thread is bounded to constructing `Dial9Context` and wrapping the entry for the queue. All encoding happens on the flush thread. Entries that have no dial9 content pay essentially nothing.

## Components

### `Dial9Context` (metrique field type)

Regular metrique struct defined in the dial9 crate:

```rust
#[metrics]
pub struct Dial9Context {
    #[metrics(field_tag(dial9::Context))]
    worker_id: WorkerId,

    #[metrics(field_tag(dial9::Context))]
    task_id: Option<TaskId>,

    #[metrics(field_tag(dial9::Context))]
    monotonic_ns_start: u64,

    /// Captures the monotonic clock at close, not at construction.
    #[metrics(field_tag(dial9::Context))]
    monotonic_ns_end: MonotonicAtClose,
}

/// Field type that reads the monotonic clock when its CloseValue runs.
/// Its closed form is u64 (monotonic nanoseconds at close).
pub struct MonotonicAtClose;

impl CloseValue for MonotonicAtClose {
    type Closed = u64;
    fn close(self) -> u64 { clock_monotonic_ns() }
}

impl Dial9Context {
    pub fn capture() -> Self { /* read worker/task/monotonic_start */ }
}
```

The metrique macro's generated `CloseValue` impl for `Dial9Context` delegates to each field's own `CloseValue::close`, which is the standard metrique pattern (the same way `Timer` and other close-time field types work). `MonotonicAtClose::close` reads the monotonic clock at close time; `u64` appears in the closed form.

Construction always captures the monotonic clock at request start. Tokio runtime state (worker id, task id) is captured if the thread is owned by a tokio runtime; otherwise those fields remain `None` / unset. `capture()` is infallible.

`Dial9Context` deliberately does not implement `Default` or `Clone`. `Default::default()` would produce zero-valued fields that silently look like "valid context captured at monotonic time 0" without ever firing the "no context" diagnostic (the `dial9::Context` tag is still present, the values are just meaningless). Users construct it with `Dial9Context::capture()` at the field initializer site; forgetting produces a clear compile error about the missing field.

The end monotonic is captured at close time via `CloseValue`, so the event carries both endpoints. Dial9 viewers can render events as timeline spans (start + end + duration) rather than single points.

If no dial9 runtime is attached at all (inert `TelemetryHandle`), `Dial9Stream` short-circuits the event; the `Dial9Context` field is harmlessly constructed and discarded.

When flattened into a user struct, the four fields become part of the parent's descriptor with the `dial9::Context` tag. Dial9 finds them by walking the descriptor at first-use.

### `Dial9Stream`

`EntryIoStream` implementor. Constructed with a `TelemetryHandle`. Runs on whatever thread metrique's pipeline calls `next` on; for the global and builder paths, that is the `BackgroundQueue` flush thread.

Per entry:

1. If the handle is inert: return `Ok(())` immediately; entries still reach EMF through the tee.
2. Look up `entry.descriptor()`. `None` is reported once (per observed concrete type id via `inner_any().type_id()`) and skipped.
3. First-use per `DescriptorId`: walk the descriptor to compute the context-field indices (fields tagged `dial9::Context`) and payload-field indices (tagged `dial9::Emit`). Build the wire schema with annotations for units.
4. Walk `entry.write(..)` with a `Dial9EntryWriter` that uses the cached index sets to route each callback to either the event header (context) or the payload encoder (Emit), or to skip. `Interned` fields have their string data routed through the dial9 string pool. Relies on the metrique contract that `Entry::write` emits `value` callbacks in descriptor order.

`Dial9EntryWriter` overrides `ValueWriter::values()` (the default implementation comma-joins elements into a string) to preserve the typed list wire encoding for `Vec<T>` fields.

A `catch_unwind(AssertUnwindSafe(..))` guard around the `Entry::write` walk drops offending events (rate-limited log) without poisoning the flush thread's state.

### Schema handling

Dial9 registers one schema per distinct `DescriptorId`. One registration per entry type, regardless of which optional fields happen to be present or which `Flex` keys appear at runtime.

Optional fields use dial9's existing optional wire encoding (high-bit optional variants on `FieldType`). `Flex` maps and `Vec`-style lists use dial9's new typed wire support (see "Trace format additions").

No shape fingerprinting on the hot path. No LRU eviction. The cache is bounded by the number of distinct descriptors the process instantiates, which is a compile-time property.

### Units

The descriptor carries `Option<Unit>` per field. Dial9 emits units as schema-level annotations, not field-name suffixes and not wire-type variants. The annotation key is `"metrique.unit"`; the value is the unit's string representation. Fields with no unit pay no annotation bytes.

For `Flex` fields, the unit applies to the map values, not the keys.

### Observability

- Periodic `tracing::debug!` reporting schema cache size and cumulative counters (registrations, events emitted, entries skipped for `None` descriptor).
- Rate-limited `tracing::warn!` on each distinct hand-written entry seen (one report per distinct concrete type id observed).

## Visualization data shape

Dial9 produces a trace with per-event data that supports viewer tooling. The design does not prescribe how the viewer renders, but the structural surface it receives from dial9 includes:

- **Timeline placement**: start and end monotonic timestamps from `Dial9Context` (start from `capture()`, end from `CloseValue`). Events render as timeline spans with duration `end - start`.
- **Worker placement**: `worker_id` from `Dial9Context`. Events pin to their starting tokio worker. Off-runtime events carry `WorkerId::UNKNOWN`.
- **Task correlation**: `task_id: Option<TaskId>` from `Dial9Context`. Lets the viewer correlate events that ran on the same task (e.g., a `DbQueryMetrics` emitted inside a `RequestMetrics` block sharing `task_id`).
- **Event type identity**: the entry's canonical name from `EntryDescriptor::name()`. Viewer tooling can group, filter, or color-code by event type.
- **Full payload with units**: every `Emit`-tagged field of the event, with:
  - Field name (post-rename, as emitted via `Entry::write`).
  - Field value (decoded per `FieldShape`: scalars, optionals, lists, dynamic maps; pooled strings transparently resolved through the trace's string pool).
  - Unit from `FieldDescriptor::unit()`, encoded as a schema-level annotation.
  - Future schema annotations (display hints, privacy labels, `dial9.kpi` markers, etc.) arrive through the same annotation mechanism.
- **Canonical wall-clock timestamp** (optional): `#[metrics(timestamp)]` fields, emitted through `EntryWriter::timestamp`. Supplements the monotonic start/end when present. Some events may not have one (monotonic is sufficient for dial9's ordering).
- **Entry schema enumeration**: descriptor-aware viewer tooling can enumerate all fields and their shapes from the `EntryDescriptor` independently of any specific event, so search/filter UI knows up-front which fields exist on which event types.

The viewer is free to render this data however fits: timeline spans, grouped lanes, inspector panels, correlation views, histograms, whatever. The design does not prescribe UI.

## Trace format additions

Two additions to `dial9-trace-format` enable the integration without per-sink extensions:

### Schema-level annotations

A new annotation section on `SchemaEntry` that carries repeated `(field_index, key, value)` tuples. Used for units today, usable for future display hints, semantic-convention labels, aggregation hints, and privacy labels without further format changes.

Units encode as `("metrique.unit", "microseconds")` on the annotated field. Fields without annotations cost nothing.

### Typed lists and maps

Two new `FieldType` variants cover the metrique shapes that cannot be represented as scalars:

- `FieldType::List(FieldType)` carries `[T]`-style list data. The element type is sealed at schema registration; the encoder does not write per-element type tags, and the decoder reads each element using the schema-bound element type. A producer that writes data inconsistent with the schema produces a corrupt stream, the same guarantee the rest of the format relies on for existing fields. Recursion is forbidden: `FieldType::List` is not a valid element type.
- `FieldType::Map { key: FieldType, value: FieldType }` represents a metrique `Flex<(String, T)>` as one schema field carrying a map at encode time, instead of one schema per runtime key. Wire layout: `<count> <repeated key value>`, using the existing scalar encodings determined by the `key` and `value` types declared in the schema. Keys and values are sealed at schema registration. Recursion is forbidden.

Pooled-string positions are expressed by setting `FieldType::PooledString` as the key, value, or list element type. The `dial9::Interned` field tag on a metrique `Flex` or list field selects the pooled variant per-position as needed.

## Error handling and resilience

- **Hand-written entries**: `descriptor()` is `None`. Dial9 reports once per distinct concrete type id observed and skips. A future extension can let hand-written entries opt in via metrique's `DescribeEntry` follow-up.
- **Entries with `Emit` fields but no `dial9::Context`-tagged fields**: dial9 treats the entry as having no context. The event header falls back to a flush-thread monotonic timestamp with `WorkerId::UNKNOWN` / `task_id = None`. A single `tracing::error!` per descriptor (deduped by `DescriptorId`, not time-rate-limited) names the offending entry type and hints that `#[cfg]` gating or forgotten `Dial9Context` field may be responsible. In debug builds this is `debug_assert!`. The payload still encodes; dropping the event would be worse.
- **Entries with `FieldShape::Opaque` selected for `Emit`**: `debug_assert!` in debug, rate-limited `tracing::error!` in release, keyed per `(DescriptorId, field)` pair; the field is skipped on the wire. The rest of the entry still encodes.
- **Inert telemetry handle**: `Dial9Stream` returns `Ok(())` immediately. Entries still reach EMF.
- **Caller thread not owned by a tokio runtime**: `Dial9Context::capture()` still records a monotonic timestamp; tokio fields remain unset. The entry encodes normally.
- **Panic inside `Value::write`**: caught per entry; the offending event is dropped with a rate-limited log. The flush thread's encoder state stays valid.

## Validation

Validation runs in two places.

### Compile-time

The metrique macro catches intrinsic structural mistakes that do not depend on dial9: conflicting `field_tag` + `field_tag(skip)`, conflicting struct-level defaults. These fire regardless of whether dial9 is in the picture.

Dial9-specific diagnostics are runtime (see below) because the metrique macro does not interpret tag identity.

### First-use (descriptor-local, per descriptor)

The first time `Dial9Stream` encounters a `DescriptorId`, it walks the descriptor for dial9-specific structural errors. The verdict caches on `DescriptorId`; each descriptor is validated at most once.

| Condition | Behaviour |
| --- | --- |
| `descriptor() == None` (hand-written entry) | rate-limited warn once per distinct concrete type id observed; entry dropped from dial9 path; EMF unaffected |
| Descriptor has `Emit` fields but no `dial9::Context`-tagged fields | `debug_assert!` in debug, single `tracing::error!` per descriptor in release (deduped by `DescriptorId`); entries of this type encode with UNKNOWN worker and flush-thread monotonic fallback |
| `Interned` on a non-string-capable shape | `debug_assert!` in debug, rate-limited `tracing::error!` in release; the offending field is skipped on the wire; rest of entry encodes |
| `FieldShape::Opaque` field tagged `Emit` | `debug_assert!` in debug, rate-limited `tracing::error!` in release; the offending field is skipped on the wire; rest of entry encodes |
| Inert `TelemetryHandle` | `Ok(())` fast path; no work; entries still reach EMF |
| Panic inside `Value::write` | event dropped; rate-limited warn; flush-thread state preserved |

None of these failure modes crash the sink in release builds. Each diagnostic includes enough context (entry type name via `EntryDescriptor::name()`, descriptor pointer as a fallback) to find the offending struct.

Periodic `tracing::debug!` reports aggregate counters: descriptors seen, descriptors skipped, events emitted, fields skipped. Off at `info` by default.

Note: the initial release does not have a binary-wide "sink attached, no dial9-compatible structs in this binary" startup check. That check depends on metrique's deferred source system (see `metrique/docs/entry-descriptors.md` → "Appendix: possible evolution, typed source extraction"). Until it reopens, dial9 relies on the first-use diagnostics above.

## Future evolution

- **Hand-written `Entry` impls opting into descriptors** (once metrique ships `DescribeEntry`) so they participate in dial9 without derive sugar.
- **Binary-wide source discovery at sink construction** (once metrique's source system re-opens). Would add a `Dial9Stream::builder().startup_discovery(true)` toggle and a warn when no dial9-bearing structs are registered.
- **Typed source extraction for context** (paired with the above). Would let `Dial9Context` be read as a typed snapshot rather than walking flattened fields. Cleaner API at the cost of more metrique-side machinery. Existing dial9 users' code continues to work unchanged.
- **Per-sink compile-time wire plans**, once metrique can emit them, to replace the flush-thread `Entry::write` walk with a direct encode.
- **More schema annotations**: display hints, aggregation hints, privacy labels, `dial9.kpi` markers for fields that should be graphed. Same mechanism as units.
- **Heterogeneous `Flex` values** once metrique carries a tagged runtime value model for them.
- **Nested container widening**: once metrique lifts its one-optional-layer restriction on `List` and `Flex.value`, dial9's `FieldType::List` and `FieldType::Map` wire variants accept the richer shapes with no format change (they already recurse at the type level).
