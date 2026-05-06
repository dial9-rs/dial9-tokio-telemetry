# Metrique integration: review-only companion

**This document is deleted as part of PR sign-off. Anything that survives review lives in `metrique-integration.md`.**

The permanent doc covers what we are building. This doc covers why we picked it and what we rejected.

See also:

- `metrique-integration-changelog.md` (review-only): what changed across rounds of PR #346 and why.
- `metrique-integration-impl-plan.md` (review-only): sequencing, dial9-side module work, and intentional scope limits.

## Requirements

Hard constraints.

1. **Heterogeneous entries.** One sink composition must handle a `BackgroundQueue` carrying multiple metrique struct types through `BoxEntry`. Users cannot be forced to split sinks per entry type.
2. **Low caller-thread overhead.** Encoding runs on the flush thread. Caller-thread cost must be bounded to capturing context and wrapping, on the order of tens of nanoseconds plus a couple of clock reads per entry.
3. **Caller-thread context capture.** `worker_id`, `task_id`, start monotonic, and end monotonic are captured where they are available, not sampled on a flush thread where the tokio thread-locals are gone.
4. **No metrique-core changes that break existing users.** Adding to metrique is acceptable; breaking existing `Entry`, `Value`, or `CloseValue` semantics is not.
5. **Uniform metrique field types.** `Duration`, `SystemTime`, `Timer`, timestamp types, `Flex`, value-string enums, and user custom types work through one path.
6. **Per-field opt-in.** Users must be able to choose which fields appear in the dial9 trace without wrapping fields in dial9-specific newtypes and without annotating every field individually.
7. **Optional-field schema stability.** A struct with optional fields produces one dial9 schema, not one per combination of present/absent optionals.
8. **`Flex` schema stability.** Dynamic-key maps produce one dial9 schema, not one per distinct key.
9. **Units are first-class.** Units survive from the metrique definition into the trace metadata without name-mangling hacks.
10. **Timeline spans, not single points.** Events should carry both start and end monotonic so viewer tooling can render them as spans.

Strong preferences:

- Single entry point for the common case (one function call builds the sink).
- Advanced compositions still work: builder variants, manual `tee` + `BackgroundQueue`.
- Clean future evolution for OTEL, custom user sinks, and a future compile-time wire plan.
- Zero cost on sinks that do not use any of this machinery.

## Non-goals

Explicitly out of scope for this dial9 release. Each has an evolution path; none is a blocker.

- **Hand-written `Entry` impls carrying dial9 telemetry.** A type with `impl Entry for MyType {}` and no `#[metrics]` returns `None` from `descriptor()` and is skipped by `Dial9Stream` with a rate-limited warn. Evolution path: metrique ships `DescribeEntry` (sketched in the metrique review). Once that lands, dial9 picks up hand-written users with no change on the dial9 side.
- **Typed source extraction for context.** An earlier draft of this design shipped a `SourceTag` trait with `type Snapshot` and a `desc.source::<C>()` API for pulling structural data out of closed entries. The metrique side deferred that system to an appendix; see `metrique/docs/entry-descriptors.md` → "Appendix: possible evolution, typed source extraction." Dial9 consequently reads context by walking the descriptor for fields marked with a `dial9::Context` tag. A second consumer (OTEL, richer dial9 integration) is the natural trigger to reopen both metrique's and dial9's source-system design. Existing dial9 user code does not need to migrate when the source system lands; the new attribute would be additive.
- **Binary-wide "no dial9-compatible structs in this binary" startup check.** Paired with the source system above. Not available without metrique's `register_descriptor` hook. Evolution path: when the metrique source system reopens, dial9 gets a `Dial9Stream::builder().startup_discovery(true)` toggle as an additive change.
- **Runtime `Entry::write` shape fingerprinting as a fallback for hand-written entries.** Decision, not deferral: a fingerprinter carries the optional-field and `Flex` explosion problems the descriptor design eliminates.
- **Compile-time generated dial9 wire plan.** The descriptor-plus-`Entry::write` path is enough to meet functional requirements. A static plan is strictly additive on top when flush-thread CPU savings matter beyond the descriptor path.
- **Programmatic stats handle in the sink.** Diagnostics use periodic `tracing::debug!` and rate-limited `tracing::warn!`. A richer stats API can land once metrique exposes a general reporting hook.
- **Schema-cache tunability.** The cache is keyed on `DescriptorId`, so its size is a compile-time property of the binary.
- **Format-layer sampling integration.** Dial9 stays an `EntryIoStream`; `FixedFractionSample` / `CongressSample` wrapping a `Format` is a follow-up.
- **User-invoked compile-time validation helper.** A sink-specific `dial9::assert_dial9_compatible!(T)` macro is not part of the design. The checks run at first-use automatically.
- **Wire format version bump.** Dial9 trace-format extensions (schema annotations, typed dynamic lists and maps) are additive. Old decoders halt at unknown tags; we accept silent truncation when a new trace is read by an older viewer because the format is not widely distributed outside this repo and the in-tree viewer ships in lockstep with producers.
- **Heterogeneous `Flex` values.** Metrique `Flex<(String, T)>` has a fixed `T` per type; dial9 mirrors that in its `Map { key, value }` field type. A tagged dynamic value form would need both sides to change.
- **Distribution-typed fields on the wire.** `metrique_aggregation::Histogram<T>`, `SharedHistogram<T>`, and user distribution types lower to `FieldShape::Opaque` today. Dial9 skips them with a diagnostic when tagged `Emit`. EMF/JSON render distributions normally. Evolution path is metrique-side.
- **Nested containers beyond one level.** `Vec<Option<T>>` and `Flex<(String, Option<T>)>` are supported; deeper combinations lower to `FieldShape::Opaque` and are therefore skipped by dial9 when tagged `Emit`. Widening the macro's recognition is additive on both sides.

## Tradeoffs worth reviewer attention

- **Schema cache keyed on `DescriptorId`.** Macro-derived descriptors are `&'static`-backed and produce stable ids; future `Arc`-backed descriptors (enum-per-variant or hand-written) produce ids stable across clones. Consequence: hand-written entries (which return `None` for the descriptor) are skipped, not encoded via a fingerprint fallback.
- **`dial9::Emit` as an opt-in tag, not an opt-out.** Users must actively tag fields to appear in the dial9 payload. The downside is that forgetting to tag means nothing shipped to dial9; the upside is per-field granularity without sink-specific newtypes. The chosen tradeoff matches rcoh's explicit feedback that "in many cases you may just want a request id for correlation and maybe a KPI or two."
- **`dial9::Context` is a dial9-internal but publicly-named tag.** Users don't interact with it directly (they flatten `Dial9Context` into their entry). The tag type is visible in dial9's public API, which leaks a private abstraction outward. Acceptable because the user-visible contract (flatten `Dial9Context`) is simpler than the underlying tag mechanism, and the tag type is not a stable guarantee: it can change if metrique gets a typed source-extraction mechanism in the future.
- **Flatten + tag resolution has four precedence levels.** Spelled out in the metrique review's "Field tag resolution: full rules." The rule that lets the dial9 example work cleanly is "flatten-site `field_tag(..)` propagates to flattened children as a default," which means `#[metrics(flatten, field_tag(skip(Emit)))] dial9: Dial9Context` propagates `skip(Emit)` into every field of `Dial9Context` without the user having to annotate each field individually.
- **Dial9 depends on a descriptor-system PR in metrique.** The dial9 implementation cannot land before [metrique#282](https://github.com/awslabs/metrique/pull/282) does. The changelog doc tracks the specific dependency.
- **Units on the wire are schema annotations, not field-name suffixes.** This is a one-time downstream-tooling change: consumers looking for `latency_Microseconds` need to look at schema metadata instead.
- **Context read by walking the descriptor at first-use, not typed extraction.** Without metrique's deferred source system, dial9 finds `Dial9Context` fields by looking for the `dial9::Context` tag on descriptor entries. Cost is a small one-time walk per `DescriptorId`; cached thereafter. The downside is a slightly fuzzier contract than `desc.source::<Dial9>()` would give. Acceptable for the initial integration; re-opens when metrique adds the source system.
- **`Entry::write` order == descriptor order as a metrique contract.** Dial9's adapter indexes into callbacks positionally. Without this contract the adapter would need to match on name, which is slower and adds indirection. The metrique impl plan specifies the contract and backs it with a CI test + debug-mode runtime check.

## Key design choices

### Descriptor-aware sink, not runtime shape inference

The sink consumes a static descriptor emitted by metrique's macro. The descriptor covers all possible fields up front, including optionality and `Flex`, so the sink never needs to infer shape from observed emissions. One registration per entry type. Runtime fingerprinting was rejected as the primary mechanism for two reasons:

1. **Optional-field schema explosion.** A struct with K optional fields can produce up to 2^K distinct observed shapes under fingerprinting. Every observed shape registers a separate schema and re-emits the schema frame. The cache either grows unboundedly or thrashes.
2. **Unbounded `Flex` keys.** Each distinct `Flex::new(key)` value produces a distinct observed shape. For keys drawn from a high-cardinality source, the cache thrashes.

Fingerprinting is not on the roadmap as a fallback for hand-written entries. See "Hand-written entries" in the Non-goals list above.

### Context capture via a flattened metrique field

Caller-thread context lives in a real metrique field (`Dial9Context`) whose constructor captures start-time worker/task/monotonic, whose `CloseValue` captures end-time monotonic, and whose fields carry a `dial9::Context` tag. The user flattens that struct into their entry with `field_tag(skip(Emit))` so the context is not duplicated into the dial9 payload.

Advantages:

- No sink wrapper in the composition path. Dial9 is a true peer sink.
- Capture runs in the entry's constructor and close path, so context is recorded on the caller thread by construction, not by convention.
- The context data is reachable via normal `Entry::write` walking; dial9's adapter routes tagged fields to the event header rather than the payload.
- End monotonic via `CloseValue` lets viewers render events as timeline spans (start + end) rather than single points. Matches yulnr's viewer-visualisation concerns.
- Users who want context visible as normal payload already get it by default (flattening emits the fields); adding `skip(Emit)` just keeps the payload from double-counting.

Two alternative shapes were considered:

- **Typed source extraction** (`#[metrics(source(Dial9))] d9: Dial9Context` with `no_write` + `desc.source::<Dial9>()`). Rejected for the initial release because the source system adds substantial metrique-side surface (`SourceTag`, hook, `linkme`-backed registration) that isn't justified by a single consumer. Deferred; see metrique review's "Deferred: typed source extraction."
- **Field-name convention** (`d9_worker_id`, `d9_task_id`, etc., discovered by name). Rejected because it tightly couples dial9 and user-chosen field names. The tag-based approach uses the existing field-tag mechanism for structural marking.

### Dial9-owned tags and user-owned opt-in

Dial9 defines `dial9::Context` (internal context marker), `dial9::Emit` (payload opt-in), and `dial9::Interned` (payload string interning) in its own crate. Users opt in via `#[metrics(...)]` attributes applied to their own structs. No metrique macro change is needed to teach the macro about dial9 specifically; the macro records opaque tag identities and the dial9 sink interprets them.

This is what makes the mechanism general. OTEL, a privacy-tier sink, a metrics-rs bridge, all use the same mechanism with their own tags.

### Schema annotations for units

Units attach to fields as schema-level annotations (`("metrique.unit", "microseconds")`). Fields with no unit pay zero bytes.

We considered:

- **Field-name suffix** (`latency_Microseconds`). Rejected: bakes units into the name, downstream consumers have to reverse it, does not support `Unit::Custom("...")` cleanly.
- **Sink-specific unit-typed fields** (`U64Microseconds`, `U64Bytes`). Rejected: scales with unit count, wire churn per new unit.

Generic schema annotations scale without wire churn and leave room for other future metadata (display hints, privacy labels, semantic-convention names, `dial9.kpi` markers for fields that should be graphed).

### Typed lists and maps for `Vec` and `Flex`

Metrique `Vec<T>` lowers to a new dial9 typed-list field; metrique `Flex<(String, T)>` lowers to a new typed-map field. One schema field regardless of runtime cardinality. Element / key / value types are sealed at schema time, matching metrique's current shapes. Heterogeneous values and deeper nesting are future extensions that need both metrique and dial9 changes.

Dial9's `Dial9EntryWriter` overrides `ValueWriter::values()` to preserve the typed list encoding. The default `ValueWriter::values()` comma-joins elements into a string, which would produce a wrong-shape schema.

### Peer sink composition unchanged

`tee(emf_stream, Dial9Stream::new(&handle))` inside a `BackgroundQueue`, `metrique_sink(...)` builder, and `ServiceMetrics::attach_to_stream_with_dial9` all remain as the three composition paths. The wire to metrique is the descriptor, not a new composition primitive.

### Validation strategy

Validation runs in two phases: compile-time intrinsic checks from the metrique macro, and per-descriptor first-use validation on the event path. Each catches a different class of error.

Compile-time checks are essentially free and entirely metrique's responsibility. They catch structural contradictions (conflicting field-tag attributes) but cannot catch dial9-specific rules because metrique treats tag identity opaquely.

First-use validation runs unconditionally, whether or not startup discovery was ever implemented. Every descriptor the sink sees on the event path gets walked once for dial9-specific structural errors (`Emit` without `dial9::Context` fields, `Interned` on non-string shape, `Opaque` in `Emit`). The verdict caches on the `DescriptorId`.

Failure policy:

- For structural errors that the sink cannot meaningfully encode (`Interned` on non-string, `Opaque` in `Emit`): `debug_assert!` in debug, rate-limited `tracing::error!` in release. These are errors the user must fix; they do not have meaningful false positives. The broken field is skipped; EMF and other sinks are unaffected.
- For soft misconfigurations (`Emit` fields present but no `dial9::Context` fields): `debug_assert!` in debug, single `tracing::error!` per descriptor in release (deduped by `DescriptorId`, not time-rate-limited so a misconfigured descriptor can't drown out others by exceeding a rate limit). The event still encodes with a flush-thread monotonic fallback.

The binary-wide "sink attached, no dial9-compatible structs in this binary" check is not in the initial release; see the Non-goals section.

## Why not compile-time (in one place)

The compile-time path keeps coming up in review, so consolidating the argument here rather than scattering it across alternatives.

**Option 1: User derives `TraceEvent` on their metrique struct.** Blocked by `BoxEntry` erasure. The concrete type is gone by the time `Dial9Stream` sees the entry. Recovering it requires one of:

- A parallel object-safe trait plus a dial9-owned box type. Object safety itself is not the blocker (as Russell noted, `Encodeable` is `dyn`-safe). The blocker is threading the sink-specific bound through `BoxEntrySink::append_any`, which takes `impl Entry + Send + 'static` and boxes internally. That bound cannot be tightened without breaking every existing user.
- A metrique-side change so `ServiceMetrics` can attach a sink typed concretely over `TraceEvent + Entry`. This propagates the sink-specific bound through every composition primitive (`tee`, `BackgroundQueue`, `FixedFractionSample`, `CongressSample`, global attach). Ecosystem fragmentation is a concrete cost, not a slogan.
- A single fixed projection on `BoxEntry`, where one method resolves to the sink-relevant data regardless of the concrete type. This is architecturally what the descriptor is: `Entry::descriptor()` is that fixed projection, chosen once for all descriptor-aware consumers rather than registered per-sink.

**Option 2: The `TraceField` blanket problem.** Every `Value` impl (primitives, unit wrappers, aggregation types) plus every user custom type would need a parallel `TraceField` impl unless a blanket `TraceField for impl Value` is provided. With the blanket, `TraceField` becomes runtime dispatch through `Value::write`, so the compile-time shape knowledge that motivated the path disappears. Without the blanket, each user type needs a hand-written `TraceField` impl forever. Either outcome is worse than reading a descriptor.

**Option 3: Metrique emits a per-sink compile-time wire plan.** This is the strongest compile-time path and we are not rejecting it forever. It would let dial9 skip `Entry::write` dispatch entirely on the flush thread: one `encode_for_dial9(entry, encoder)` function per entry type.

We are not building it now because:

- The descriptor carries strictly less information than a static plan would need. A plan layer can be added on top of the descriptor without breaking anything.
- The dial9 cost on the flush thread is already off the caller's critical path (`BackgroundQueue` absorbs latency). The marginal win from a static plan is CPU on the flush thread, not latency on the caller thread.
- Shipping the plan requires metrique to either understand dial9's wire format (bad: tight coupling) or generate a sink-pluggable plan that dial9 consumes (more design work, not shorter than the descriptor design).

**Bottom line.** Compile-time would buy us flush-thread CPU and nothing else. It would cost us `BoxEntry` compatibility or ecosystem fragmentation, and it would still need a descriptor-or-equivalent to handle optional fields, `Flex`, units, and context extraction. The descriptor path is the one that survives after you cross all those items off.

## Alternatives considered

### A: Runtime shape fingerprinting (the original PR)

Rejected. See "Descriptor-aware sink, not runtime shape inference" above.

### B: Dial9-specific `#[derive(TraceEvent)]` on the same struct

Rejected. See "Why not compile-time (in one place)" above.

### C: Compile-time dial9 wire plan

Deferred rather than rejected. See "Why not compile-time (in one place)" above.

### D: `D9Meta` / `Dial9Meta` with a `Default` impl and flatten-only sugar

Partially accepted. The flatten-plus-tag shape the initial release uses is essentially this, with the addition of a `dial9::Context` tag so the sink finds context fields structurally rather than by name.

### E: Dial9 as a pure `Format`

Partially accepted (`Dial9Stream` is an `EntryIoStream`), but the pure-`Format` variant is rejected because `Format::format` runs on the flush thread and cannot capture tokio context from there. Capture has to happen in the caller-thread construction of the entry.

### F: Wrapping metrique's `BackgroundQueue` / `tee` primitives in dial9-owned types

Rejected. Fragments the ecosystem; offers marginal enforcement beyond what the global and builder paths already provide; complicates composition with other metrique-writer features.

### G: Separate dial9-owned background thread

Rejected. Duplicates the work metrique already does via `BackgroundQueue`. Requires `Clone` on entries. Makes flush/shutdown semantics harder.

### H: Units in field names

Rejected. See "Schema annotations for units" above.

### I: Units as sink-specific wire types

Rejected. See "Schema annotations for units" above.

### J: `Flex` keys always interned

Rejected. `Flex` keys are user-controlled and may be high-cardinality. Interning is an opt-in field tag (`Interned`), not a default.

### K: Programmatic stats handle

Deferred. Shape of the desired stats API is unclear without real usage; the `tracing::debug!` periodic reporting plus rate-limited `tracing::warn!` on the error paths is enough for diagnosis. Integration into a proper metrics pipeline can happen once metrique exposes a better reporting hook (see [metrique#205](https://github.com/awslabs/metrique/issues/205)).

### L: User-invoked compile-time validation macro

Rejected. An opt-in compile-time check is a runtime check for anyone who does not remember to invoke it, which is most users most of the time. It makes the "compile-time" label dishonest. The checks already run at first-use.

### M: Typed source extraction for context (`#[metrics(source(Dial9))]`)

Deferred to metrique's source-system evolution. The initial release uses the `dial9::Context` tag + flatten approach instead. See "Context capture via a flattened metrique field" above and the metrique review's "Deferred: typed source extraction" section. When a second consumer materialises (OTEL, richer dial9 integration), the source system reopens and dial9's context capture migrates to typed extraction at that time. Existing dial9 user code does not need to change.

### N: Binary-wide startup-time "no matching structs" discovery

Deferred with the source system. Paired with `SourceTag::register_descriptor` hook. Not available in the initial release.

### O: `TokioContextSink` wrapper around the outer sink

The round-1 design captured caller-thread context through a sink wrapper that injected an `EntryConfig`. Rejected because it made dial9 a privileged sink in the composition. The round-2+ design puts capture in `Dial9Context` as a real metrique field, so dial9 is a peer sink and users can see the capture pattern explicitly in their own struct definition.

## Feasibility checks

- `BoxEntry::inner()` returns `&(dyn Any + Send + 'static)`; the concrete closed entry is reachable for future source extraction when that system re-opens.
- Adding `descriptor()` as a defaulted method on the `Entry` trait is a metrique-side SemVer minor change. Dial9 depends on it.
- `tee` and `BackgroundQueue` are public; the existing composition paths continue to work unchanged.
- `EntryConfig` is retained; it is the right primitive for per-emission, sink-provided data. Descriptors cover per-type, entry-provided data. The two coexist.
- `dial9_tokio_telemetry::telemetry::clock_monotonic_ns()` is `pub` and callable from any thread; `Dial9Context::capture()` and `Dial9Context`'s `CloseValue` use it directly.
- Schema annotations and typed dynamic lists/maps are additive extensions of `dial9-trace-format`. We do not bump the format version. Old decoders hitting an unknown top-level frame tag or an unknown `FieldType` variant return `None` and halt; new traces silently truncate at the first extension when read with an older viewer. We accept that behaviour because the format is not widely distributed outside this repo, the viewer ships in-tree, and users producing new traces are in the same update cycle as the viewer.
- `ValueWriter::values()` can be overridden; dial9's override preserves the typed list encoding. Documented explicitly in the keeper.
