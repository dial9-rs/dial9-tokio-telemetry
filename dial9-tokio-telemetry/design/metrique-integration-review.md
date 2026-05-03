# Metrique integration: review-only companion

**This document is deleted as part of PR sign-off. Anything that survives review lives in `metrique-integration.md`.**

The permanent doc covers what we are building. This doc covers why we picked it and what we rejected.

See also:

- `metrique-integration-changelog.md` (review-only): what changed since round 1 of PR #346 and why.
- `metrique-integration-impl-plan.md` (review-only): sequencing, metrique-side and dial9-side module work, and intentional scope limits.

## Requirements

Hard constraints. Any design violating these would be rejected.

1. **Heterogeneous entries.** One sink composition must handle a `BackgroundQueue` carrying multiple metrique struct types through `BoxEntry`. Users cannot be forced to split sinks per entry type.
2. **Low caller-thread overhead.** Encoding runs on the flush thread. Caller-thread cost must be bounded to capturing context and wrapping, on the order of tens of nanoseconds plus one clock read per entry.
3. **Caller-thread context capture.** `worker_id`, `task_id`, and the event start timestamp are captured where they are available, not sampled on a flush thread where the tokio thread-locals are gone.
4. **No metrique-core changes that break existing users.** Adding to metrique is acceptable; breaking existing `Entry`, `Value`, or `CloseValue` semantics is not.
5. **Uniform metrique field types.** `Duration`, `SystemTime`, `Timer`, timestamp types, `Flex`, value-string enums, and user custom types work through one path.
6. **Per-field opt-in.** Users must be able to choose which fields appear in the dial9 trace without wrapping fields in dial9-specific newtypes and without annotating every field individually.
7. **Optional-field schema stability.** A struct with optional fields produces one dial9 schema, not one per combination of present/absent optionals.
8. **Flex schema stability.** Dynamic-key maps produce one dial9 schema, not one per distinct key.
9. **Units are first-class.** Units survive from the metrique definition into the trace metadata without name-mangling hacks.

Strong preferences:

- Single entry point for the common case (one function call builds the sink).
- Advanced compositions still work: builder variants, manual `tee` + `BackgroundQueue`.
- Clean future evolution for OTEL, custom user sinks, and a future compile-time wire plan.
- Compile-time misconfiguration detection where cheap.
- Zero cost on sinks that do not use any of this machinery.

## Tradeoffs worth reviewer attention

- **Schema cache keyed on `&'static EntryDescriptor` pointer, not shape fingerprint.** This is correct because macro-derived descriptors are `'static` and unique per type. It works because the descriptor is the full closed-shape description; sinks never need to observe emissions to learn structure. Consequence: hand-written entries (which return `None` for the descriptor) are skipped, not encoded via a fingerprint fallback.
- **`no_emit` introduces a new close-time behaviour users have to understand.** `ignore` and `no_emit` are adjacent but distinct: `ignore` means "the macro pretends this field does not exist for metrics purposes"; `no_emit` means "keep the field closed and retained, but do not emit through `EntryWriter`." The diagnostic story has to be careful to push users to the right one.
- **`InTrace` default inheritance interacts with `flatten`.** The rule we landed on (child explicit decisions win; parent defaults fill only unspecified) is the rule that lets `Dial9Context` protect its own fields from accidental `InTrace` inheritance when flattened. It is a rule reviewers should exercise against their own use cases.
- **Dial9 depends on a descriptor-system PR in metrique.** The dial9 implementation cannot land before the metrique PR does. The changelog doc tracks the specific dependency.
- **Units on the wire are schema annotations, not field-name suffixes.** This is a one-time downstream-tooling change: consumers looking for `latency_Microseconds` need to look at schema metadata instead. It is the right long-term shape; field-name suffix is a hack we did not want to permanently bake in.
- **Hand-written entries are skipped.** We accept losing them for this pass. The evolution path is a metrique-level "hand-written `DescribeEntry`" extension, not a dial9-level fallback. A fallback would duplicate most of the design's motivation.

## Key design choices

### Descriptor-aware sink, not runtime shape inference

The original PR walked `Entry::write` and inferred schema from observed `(name, field_type)` sequences. That approach was rejected as the primary mechanism for two reasons:

1. **Optional-field schema explosion.** A struct with K optional fields can produce up to 2^K distinct observed shapes. Every observed shape registers a separate schema and re-emits the schema frame. The cache either grows unboundedly or thrashes.
2. **Unbounded Flex keys.** Each distinct `Flex::new(key)` value produces a distinct observed shape. For keys drawn from a high-cardinality source, the cache thrashes.

The revised design consumes a static descriptor from metrique. Descriptor covers all possible fields up front. One registration per entry type. Optional fields and Flex lower to explicit descriptor entries, so the sink never needs to infer.

Runtime discovery is still available as a fallback for hand-written entries; we chose to skip them in v1 instead of paying for two code paths.

### Context capture via a metrique source field, not a sink wrapper

An earlier iteration captured caller-thread context through a `TokioContextSink` wrapper that injected an `EntryConfig`. The revised design puts capture in a real metrique field (`Dial9Context`) whose constructor reads the tokio thread-locals and whose closed form is the snapshot the sink extracts via `Source<Dial9>`.

Advantages:

- No sink wrapper in the composition path. Dial9 is a true peer sink.
- Capture runs in the entry's constructor, so context is recorded on the caller thread by construction, not by convention.
- The closed snapshot survives `BoxEntry` erasure in a typed way because it is reachable through `inner_any` and `Source<Dial9>`.
- Users who want context visible as normal payload can `flatten` instead of `no_emit`; the source data remains structurally available.

The sink wrapper is not removed outright: users who want runtime-wide defaults can still provide their own helper that constructs `Dial9Context` and merges it in. It stops being the primary path.

### Dial9-owned tags and provider, user-owned opt-in

Dial9 defines `Dial9` (source tag), `InTrace` (field tag), and `InternString` (field tag) in its own crate. Users opt in via `#[metrics(...)]` attributes applied to their own structs. No metrique macro change is needed to teach the macro about dial9 specifically; the macro records opaque tag identities and the dial9 sink interprets them.

This is what makes the mechanism general. OTEL, a privacy-tier sink, a metrics-rs bridge, all use the same mechanism with their own tags.

### Schema annotations for units

Units attach to fields as schema-level annotations (`("metrique.unit", "microseconds")`). Fields with no unit pay zero bytes.

We considered:

- **Field-name suffix** (`latency_Microseconds`). Rejected: bakes units into the name, downstream consumers have to reverse it, does not support `Unit::Custom("...")` cleanly.
- **Sink-specific unit-typed fields** (`U64Microseconds`, `U64Bytes`). Rejected: scales with unit count, wire churn per new unit.

Generic schema annotations scale without wire churn and leave room for other future metadata (display hints, privacy labels, semantic-convention names).

### Typed dynamic maps for Flex

Metrique `Flex<(String, T)>` lowers to a new dial9 typed-map field, encoded as `<count> <repeated key value>`. One schema field regardless of keys. The value type is fixed at schema time, matching metrique's current Flex shape. Heterogeneous values are a future extension that needs both metrique and dial9 changes.

### Peer sink composition unchanged

`tee(emf_stream, Dial9Stream::new(&handle))` inside a `BackgroundQueue`, `metrique_sink(...)` builder, and `ServiceMetrics::attach_to_stream_with_dial9` all remain as the three composition paths. The wire to metrique is the descriptor, not a new composition primitive.

## Alternatives considered

### Alternative A: Runtime shape fingerprinting (the original PR)

Rejected. See "Descriptor-aware sink, not runtime shape inference" above.

Kept as a bounded fallback for hand-written entries, then cut to "skip hand-written entries" to avoid two code paths. Can be reinstated if hand-written-entry support becomes important.

### Alternative B: Dial9-specific `#[derive(TraceEvent)]` on the same struct

Rejected. Two independent blockers.

1. **`BoxEntry` erasure.** `BoxEntrySink::append_any` takes `impl Entry + Send + 'static` and boxes internally. By the time `Dial9Stream` sees the entry, the concrete type (and any `TraceEvent` impl) has been erased. Preserving `TraceEvent`-ness requires either a parallel object-safe trait plus a dial9-owned box, or a metrique-side change so `ServiceMetrics` can attach a sink typed over `TraceEvent + Entry`. In Russell's review thread, he pointed out that `Encodeable` is `dyn`-safe; the blocker is not object safety but threading the type through `BoxEntrySink::append_any`, which does not carry that bound. The least-bad workaround is a TypeId-keyed vtable bridge on `BoxEntry`, which does not feel right.
2. **Parallel `TraceField` maintenance.** Every `Value` impl (primitives, unit wrappers, aggregation types) plus every user custom type would need a parallel `TraceField` impl. Russell suggested a blanket `TraceField for impl Value`, which works for the primitives path, but it loses compile-time shape knowledge for user types (the whole reason one would use `TraceEvent` in the first place).

The descriptor path sidesteps both. It reads metrique's stable abstraction and does not require any sink-specific trait on `Value` impls.

### Alternative C: Compile-time dial9 wire plan

Rejected for this pass. A compile-time wire plan would let dial9 skip `Entry::write` dispatch on the flush thread. It would be strictly better for performance, but it does not unlock any of the functional requirements; the descriptor path already meets them.

Static plans are an evolution path, not a prerequisite. The descriptor already carries strictly less information than a static plan would; adding a plan layer on top is additive.

### Alternative D: `D9Meta` / `Dial9Meta` with a `Default` impl and flatten-only sugar

Russell's proposal: users write `#[metrics(flatten)] d9: D9Meta` with `..Default::default()` on construction, and the sink picks D9Meta out of the flattened fields.

Rejected as the primary path because:

- It conflates source semantics with field emission. Context data does not always belong in normal emission.
- The sink identifying "this is the dial9 context" by convention is fragile. A typed `Source<Dial9>` extractor is a better contract.
- `flatten` was never intended as a hook for sink-specific extraction; repurposing it narrows metrique's flexibility.

We kept flatten as a secondary path for users who want dial9 context **and** normal emission. That still works because `Dial9Context` carries its own `default_field_tag(skip(InTrace))` so the parent's `InTrace` default does not accidentally pull its fields into the dial9 payload.

### Alternative E: Dial9 as a pure `Format`

Partially accepted (`Dial9Stream` is an `EntryIoStream`), but the pure-`Format` variant is rejected because `Format::format` runs on the flush thread and cannot capture tokio context from there. Capture has to happen in the caller-thread construction of the entry.

### Alternative F: Wrapping metrique's `BackgroundQueue` / `tee` primitives in dial9-owned types

Rejected. Fragments the ecosystem; offers marginal enforcement beyond what the global and builder paths already provide; complicates composition with other metrique-writer features.

### Alternative G: Separate dial9-owned background thread

Rejected. Duplicates the work metrique already does via `BackgroundQueue`. Requires `Clone` on entries. Makes flush/shutdown semantics harder.

### Alternative H: Units in field names

Rejected. See "Schema annotations for units" above.

### Alternative I: Units as sink-specific wire types

Rejected. See "Schema annotations for units" above.

### Alternative J: Flex keys always interned

Rejected. Flex keys are user-controlled and may be high-cardinality. Interning is an opt-in field tag (`InternString`), not a default.

### Alternative K: Programmatic stats handle in v1

Rejected for this pass. Shape of the desired stats API is unclear without real usage; the `tracing::debug!` periodic reporting plus rate-limited `tracing::warn!` on the error paths is enough for diagnosis. Integration into a proper metrics pipeline can happen once metrique exposes a better reporting hook (see [metrique#205](https://github.com/awslabs/metrique/issues/205)).

## Feasibility checks

- `BoxEntry::inner()` returns `&(dyn Any + Send + 'static)`; the concrete closed entry is reachable for `Source<Dial9>` extraction.
- Adding one method (`descriptor()`) to the erased entry trait is a metrique-side surface change, not a dial9-side one. Dial9 depends on it; see the impl plan for sequencing.
- `tee` and `BackgroundQueue` are public; the existing composition paths continue to work unchanged.
- `EntryConfig` is retained; it is the right primitive for per-emission, sink-provided data. Descriptors and sources cover per-type, entry-provided data. The two coexist.
- `dial9_tokio_telemetry::telemetry::clock_monotonic_ns()` is `pub` and callable from any thread; `Dial9Context::capture()` uses it directly.
- Schema annotations and typed dynamic maps are additions to `dial9-trace-format`. Backward compatibility is achievable with a schema-section version bump; see the impl plan for the concrete shape.
