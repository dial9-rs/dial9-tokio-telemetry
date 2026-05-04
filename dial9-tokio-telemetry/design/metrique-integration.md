# Metrique integration

> **Status: design, not yet implemented.**

Dial9 is a peer metrique sink. Users configure dial9 alongside their existing EMF/JSON metrique pipeline; every metrique entry that flows through the configured sink is also recorded into the dial9 trace. A single trace file carries both tokio runtime telemetry and per-request application metrics.

The sink reads metrique's entry descriptor for each entry to learn its structural shape (fields, optionality, Flex, units), extracts caller-thread context via metrique's source system, and encodes the user-selected subset of fields into the dial9 trace. Nothing about the integration requires a dial9-specific metrique macro or dial9-specific newtype wrappers on fields.

This design depends on the entry descriptor system in metrique (see `docs/entry-descriptors.md` in the metrique repo; tracked under [metrique PR TBD](https://github.com/awslabs/metrique/pulls)). The dial9 side is a descriptor-aware sink; the metrique side is where descriptors, sources, and field tags are defined.

## User-facing API

### Opt-in on the entry

```rust
use dial9::{Dial9, Dial9Context, InTrace, InternString};

#[metrics(default_field_tag(InTrace))]
struct RequestMetrics {
    #[metrics(no_write)]
    dial9: Dial9Context,

    #[metrics(field_tag(InternString))]
    route: String,

    operation: &'static str,
    request_id: String,

    #[metrics(field_tag(skip(InTrace)))]
    debug_blob: String,
}
```

What this means:

- `Dial9Context` is a dial9-provided metrique field type. Its constructor captures caller-thread context (worker id, task id, start monotonic timestamp). It is declared `#[metrics(source(Dial9))]` in the dial9 crate, so the sink can extract a snapshot from the closed entry.
- `no_write` retains `Dial9Context` on the closed entry so the sink can read it, but does not emit its fields through `Entry::write`. `Dial9Context` itself carries `default_field_tag(skip(InTrace))` in its own definition, so if a user opts for `flatten` instead, its fields do not accidentally get pulled into the dial9 payload by the parent's `InTrace` default.
- `InTrace` marks fields that should appear in the dial9 trace payload. `skip(InTrace)` at the struct level inverts the default.
- `InternString` tells the sink to route string data in this field through dial9's string pool.

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

### Flatten as an alternative for `Dial9Context`

If the user wants dial9 context visible in normal (non-dial9) emissions too:

```rust
#[metrics(default_field_tag(InTrace))]
struct RequestMetrics {
    #[metrics(flatten)]
    dial9: Dial9Context,
    // ...
}
```

Because `Dial9Context` itself declares `default_field_tag(skip(InTrace))`, its fields are not tagged `InTrace` by parent-default inheritance. They still carry structural source data reachable via `desc.source::<Dial9>(..)`.

## Architecture

```text
┌────────────────────────────────────────────────────────────────┐
│ COMPILE TIME: metrique macro                                   │
│                                                                │
│ Sink-side (in dial9 crate):                                    │
│   #[metrics(source(Dial9))]                                    │
│   #[metrics(default_field_tag(skip(InTrace)))]                 │
│   pub struct Dial9Context { /* worker/task/monotonic */ }      │
│                                                                │
│ User-side:                                                     │
│   #[metrics(default_field_tag(InTrace))]                       │
│   struct RequestMetrics {                                      │
│       #[metrics(no_write)]      dial9: Dial9Context,            │
│       #[metrics(field_tag(InternString))] route: String,       │
│       ...                                                      │
│   }                                                            │
│                                                                │
│ Macro emits:                                                   │
│   impl Entry for ClosedRequestMetrics (as today)               │
│   static EntryDescriptor (fields, tags, units, sources)        │
│   SourceExtractor for Dial9 stored in descriptor                  │
│   descriptor() hook on the erased entry vtable                 │
└────────────────────────────────────────────────────────────────┘
                              │
                              ▼
┌────────────────────────────────────────────────────────────────┐
│ CALLER THREAD: request path                                    │
│                                                                │
│ let m = RequestMetrics { dial9: Dial9Context::capture(), ... };│
│   Dial9Context::capture() reads:                               │
│     tokio worker id, task id, monotonic clock                  │
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
│ Dial9Context closes to ClosedDial9Context, retained on the     │
│ closed entry because of no_write.                               │
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
│         fast path checks:                                      │
│           d has no InTrace fields and no Dial9 source? drop.   │
│           d has InTrace fields but no Dial9 source? report.    │
│                                                                │
│         schema = schema_cache.entry(d).or_insert_with(|| {     │
│             build_schema_from_descriptor(d)                    │
│         });                                                    │
└────────────────────────────────────────────────────────────────┘
                              │
                              ▼
┌────────────────────────────────────────────────────────────────┐
│ FLUSH THREAD: inside Dial9Stream, per entry                    │
│                                                                │
│ ctx = d.source::<Dial9>(entry.inner_any())                     │
│ encoder.start_event(                                           │
│     timestamp = ctx.start_monotonic_ns,                        │
│     worker    = ctx.worker_id,                                 │
│     task      = ctx.task_id,                                   │
│     schema,                                                    │
│ )                                                              │
│                                                                │
│ entry.write(Dial9EntryWriter {                                 │
│     descriptor: d,                                             │
│     schema,                                                    │
│     encoder,                                                   │
│ }):                                                            │
│                                                                │
│   walk Entry::write in descriptor order. For each (name,       │
│   value), consult the descriptor:                              │
│                                                                │
│     InTrace absent?                                            │
│       -> skip                                                  │
│                                                                │
│     InTrace present?                                           │
│       -> encode according to FieldShape:                       │
│            Known   : encode scalar                             │
│            Optional: encode presence byte + inner              │
│            List    : encode <count> <repeated element>         │
│            Flex    : encode map<key, value>                    │
│            Opaque  : report + skip (sink-side validation)      │
│                                                                │
│     InternString present and field carries string data?        │
│       -> route through encoder.intern_string(..)               │
│                                                                │
│ encoder.end_event()                                            │
└────────────────────────────────────────────────────────────────┘
```

Work on the caller thread is bounded to constructing `Dial9Context` and wrapping the entry for the queue. All encoding happens on the flush thread. Entries that have no dial9 content pay essentially nothing.

## Components

### `Dial9Context` (metrique field type)

Regular metrique struct defined in the dial9 crate:

```rust
#[metrics(source(Dial9))]
#[metrics(default_field_tag(skip(InTrace)))]
pub struct Dial9Context { /* private */ }

impl Dial9Context {
    pub fn capture() -> Self { /* read worker/task/monotonic */ }
}
```

Construction always captures the monotonic clock. Tokio runtime state (worker id, task id) is captured if the thread is owned by a tokio runtime; otherwise those fields remain `None` / unset. `capture()` is infallible: an off-runtime call is a legitimate "no tokio context right here" signal, not an error. The closed form is the snapshot the sink extracts via `desc.source::<Dial9>(entry.inner_any())`.

If no dial9 runtime is attached at all (inert `TelemetryHandle`), `Dial9Stream` short-circuits the event; the `Dial9Context` field is harmlessly constructed and discarded. No rate-limited warn is required for the off-runtime case; the captured snapshot carries a timestamp and whatever tokio state was available.

### `Dial9Stream`

`EntryIoStream` implementor. Constructed with a `TelemetryHandle`. Runs on whatever thread metrique's pipeline calls `next` on; for the global and builder paths, that is the `BackgroundQueue` flush thread.

Per entry:

1. If the handle is inert: return `Ok(())` immediately; entries still reach EMF through the tee.
2. Look up `entry.descriptor()`. `None` is reported once and skipped.
3. Look up the entry's `Dial9` snapshot via `desc.source::<Dial9>(entry.inner_any())`. Missing source with present `InTrace` fields is reported and the entry is skipped.
4. Ensure a schema is registered for this descriptor (see "Schema handling" below).
5. Start an event on the encoder using the snapshot timestamp.
6. Walk `Entry::write` with a `Dial9EntryWriter` that uses the descriptor to filter by `InTrace`, route `InternString` fields through the string pool, and encode each value according to its `FieldShape`.
7. End the event.

A `catch_unwind(AssertUnwindSafe(..))` guard around the `Entry::write` walk drops offending events (rate-limited log) without poisoning the flush thread's state.

### Schema handling

Dial9 registers one schema per distinct `EntryDescriptor`. The descriptor is `'static`, so the cache key is a pointer comparison. One registration per descriptor, regardless of which optional fields happen to be present or which Flex keys appear.

Optional fields use dial9's existing optional wire encoding (high-bit optional variants on `FieldType`). Flex maps use dial9's new typed-map wire support (see "Trace format additions").

No shape fingerprinting on the hot path. No LRU eviction. The cache is bounded by the number of distinct descriptors the process instantiates, which is a compile-time property.

### Units

`FieldDescriptor::unit: Option<Unit>` reaches the sink through the descriptor. Dial9 emits units as schema-level annotations, not field-name suffixes and not wire-type variants. The annotation key is `"metrique.unit"`; the value is the unit's string representation, including `Unit::Custom("...")` cases. Fields with no unit pay no annotation bytes.

For Flex fields, the unit applies to the map values, not the keys.

### Observability

- Periodic `tracing::debug!` reporting schema cache size and cumulative counters (registrations, events emitted, entries skipped for `None` descriptor, entries skipped for missing source).
- On first-use detection of a structurally broken descriptor (InTrace without Dial9 source, InternString on a non-string shape, Opaque field tagged InTrace): `debug_assert!` panic in debug, rate-limited `tracing::error!` in release, keyed per descriptor so each distinct broken type is reported once. Rate-limited `tracing::warn!` on each distinct hand-written entry observed (no descriptor to validate against).

## Trace format additions

Two additions to `dial9-trace-format` enable the integration without per-sink extensions:

### Schema-level annotations

A new annotation section on `SchemaEntry` that carries repeated `(field_index, key, value)` tuples. Used for units today, usable for future display hints, semantic-convention labels, aggregation hints, and privacy labels without further format changes.

Units encode as `("metrique.unit", "microseconds")` on the annotated field. Fields without annotations cost nothing.

### Typed lists and maps

Two new `FieldType` variants cover the metrique shapes that cannot be represented as scalars:

- `FieldType::List(FieldType)` carries `[T]`-style list data. The element type is sealed at schema registration; the encoder does not write per-element type tags, and the decoder reads each element using the schema-bound element type. A producer that writes data inconsistent with the schema produces a corrupt stream, the same guarantee the rest of the format relies on for existing fields. Recursion is forbidden: `FieldType::List` is not a valid element type.
- `FieldType::Map { key: FieldType, value: FieldType }` represents a metrique `Flex<(String, T)>` as one schema field carrying a map at encode time, instead of one schema per runtime key. Wire layout: `<count> <repeated key value>`, using the existing scalar encodings determined by the `key` and `value` types declared in the schema. Keys and values are sealed at schema registration. Recursion is forbidden: `FieldType::Map` is not a valid `key` or `value` type. Map-of-map is out of scope; a future extension can introduce a tagged-value form if metrique grows heterogeneous dynamic values.

Pooled-string positions are expressed by setting `FieldType::PooledString` as the key or the value (or the element type for `List`). The `dial9::InternString` field tag on a metrique `Flex` or list field selects the pooled variant per-position as needed.

## Error handling and resilience

- **Hand-written entries**: `descriptor()` is `None`. Dial9 reports once per distinct type id observed (via `inner_any().type_id()`) and skips. A future extension can let hand-written entries opt in.
- **Entries with `InTrace` fields but no `Dial9` source**: reported once per descriptor; entries are skipped. This is a user configuration error; the sink surfaces it rather than encoding partial events.
- **Entries with `FieldShape::Opaque` selected for `InTrace`**: reported once per `(descriptor, field)` pair; the field is skipped on the wire. The rest of the entry still encodes.
- **Inert telemetry handle**: `Dial9Stream` returns `Ok(())` immediately. Entries still reach EMF.
- **Caller thread not owned by a tokio runtime**: `Dial9Context::capture()` still records a monotonic timestamp; tokio fields remain unset. The entry encodes normally.
- **Panic inside `Value::write`**: caught per entry; the offending event is dropped with a rate-limited log. The flush thread's encoder state stays valid.

## Validation

Three phases, all automatic. Users configure no compile-time helpers and invoke no runtime registration APIs.

### Compile-time (metrique macro, intrinsic)

The metrique macro catches structural mistakes that do not depend on dial9: duplicate source tags, conflicting `field_tag` + `field_tag(skip)`, conflicting struct-level defaults, and source tags that do not implement `SourceTag`. These diagnostics fire regardless of whether dial9 is in the picture.

### Startup-time (binary-wide, opt-out via feature)

Dial9 implements `metrique::SourceTag` for its `Dial9` tag. Every macro-derived entry declaring `source(Dial9)` registers its `&'static EntryDescriptor` into a dial9-owned vec before `main`. At sink construction, dial9:

1. If the registered vec is empty: `debug_assert!` panic in debug builds; rate-limited `tracing::warn!` in release. Release does not abort. The message points the user at "you attached a dial9 sink, but no struct in this binary declares `source(Dial9)`; the sink will not produce any events."
2. If the registered vec is non-empty: run per-descriptor structural checks on every registered descriptor (same checks as first-use, described below). Any failures report via `debug_assert!` in debug builds and rate-limited `tracing::error!` in release.

Known false-positive and false-negative scenarios:

- **False negative (warn fires when user would not expect)**: multi-binary workspaces where the tagged struct lives in a binary other than the one with the sink. The binary with the sink really does have no entries; the warn is technically correct.
- **False negative**: feature-gated or `#[cfg]`-hidden structs that are not compiled into the current binary.
- **False positive (warn does not fire when user has a misconfiguration)**: a dependency ships its own tagged entries. The user's binary has entries from the dep even though the user added none of their own.
- **Exotic build setups**: unusual linker flags that strip pre-main registration sections. In these builds the warn always fires regardless of user code.

Users who hit a legitimate false negative on a supported target can disable the empty-registry warn per-sink via the builder:

```rust
Dial9Stream::builder(&handle).startup_discovery(false).build();
```

Per-descriptor first-use validation (below) continues to run unconditionally.

On targets where link-time registration is unavailable (WASM without the relevant feature flags, exotic embedded targets), dial9's `SourceTag` override is cfg'd out. No registrations are emitted, no registry is iterated, the empty-registry warn is compiled out. First-use validation still runs.

### First-use (descriptor-local, per descriptor, always on)

The first time `Dial9Stream` sees a descriptor in the event path (rather than via startup registration, which may be skipped), it walks the descriptor for self-contradictions. The check is cached on the `&'static EntryDescriptor` pointer; each descriptor is validated at most once.

| Condition | Behaviour |
| --- | --- |
| `descriptor() == None` (hand-written entry, no `DescribeEntry`) | rate-limited warn once per observed concrete type; entry dropped from dial9 path; EMF unaffected |
| Descriptor has `InTrace` fields but no `Dial9` source | `debug_assert!` in debug, rate-limited `tracing::error!` in release; entries of this type dropped from dial9 path |
| `InternString` on a non-string-capable shape | `debug_assert!` in debug, rate-limited `tracing::error!` in release; the offending field is skipped on the wire; rest of entry encodes |
| `FieldShape::Opaque` field tagged `InTrace` | `debug_assert!` in debug, rate-limited `tracing::error!` in release; the offending field is skipped on the wire; rest of entry encodes |
| Inert `TelemetryHandle` | `Ok(())` fast path; no work; entries still reach EMF |
| Panic inside `Value::write` | event dropped; rate-limited warn; flush-thread state preserved |

None of these failure modes crash the sink in release builds. Each diagnostic includes enough context (entry type name when available, descriptor pointer as a fallback) to find the offending struct.

Periodic `tracing::debug!` reports aggregate counters: descriptors seen, descriptors skipped, events emitted, fields skipped. Off at `info` by default.

## Future evolution

- Hand-written `Entry` impls opting into descriptors (once metrique ships `DescribeEntry`) so they participate in dial9 without derive sugar.
- Per-sink compile-time wire plans, once metrique can emit them, to replace the flush-thread `Entry::write` walk with a direct encode.
- More schema annotations: display hints, aggregation hints, privacy labels. Same mechanism as units.
- Heterogeneous Flex values once metrique carries a tagged runtime value model for them.
- Nested container widening: once metrique lifts its one-optional-layer restriction on `List` and `Flex.value`, dial9's `FieldType::List` and `FieldType::Map` wire variants accept the richer shapes with no format change (they already recurse at the type level).
