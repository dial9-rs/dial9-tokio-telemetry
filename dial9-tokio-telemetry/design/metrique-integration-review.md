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

## Non-goals

Explicitly out of scope for this dial9 release. Each has an evolution path; none is a blocker.

- **Hand-written `Entry` impls carrying dial9 telemetry.** A type with `impl Entry for MyType {}` and no `#[metrics]` returns `None` from the erased `descriptor()` and is skipped by `Dial9Stream` with a rate-limited warn. Evolution path: metrique ships `DescribeEntry` (sketched in the metrique review as a non-goal of the initial metrique release). Once that lands, dial9 picks up hand-written users with no change on the dial9 side.
- **Runtime `Entry::write` shape fingerprinting as a fallback for hand-written entries.** Decision, not deferral: a fingerprinter carries the optional-field and Flex explosion problems the descriptor design eliminates.
- **Compile-time generated dial9 wire plan.** The descriptor-plus-`Entry::write` path is enough to meet functional requirements. A static plan is strictly additive on top when flush-thread CPU savings matter beyond the descriptor path.
- **Programmatic stats handle in the sink.** Diagnostics use periodic `tracing::debug!` and rate-limited `tracing::warn!`. A richer stats API can land once metrique exposes a general reporting hook.
- **Schema-cache tunability.** The cache is keyed on `&'static EntryDescriptor` pointers, so its size is a compile-time property. No tuning surface.
- **Format-layer sampling integration.** Dial9 stays an `EntryIoStream`; `FixedFractionSample` / `CongressSample` wrapping a `Format` is a follow-up that needs either a new composition shape or a metrique-side change.
- **User-invoked compile-time validation helper.** A sink-specific `dial9::assert_dial9_compatible!(T)` macro is not part of the design. The checks run at first-use and at startup-time discovery automatically.
- **Wire format version bump.** Dial9 trace-format extensions (schema annotations, typed dynamic maps) are additive. Old decoders halt at unknown tags; we accept silent truncation when a new trace is read by an older viewer because the format is not widely distributed outside this repo and the in-tree viewer ships in lockstep with producers.
- **Heterogeneous Flex values.** Metrique `Flex<(String, T)>` has a fixed `T` per type; dial9 mirrors that in its `Map { key, value }` field type. A tagged dynamic value form would need both sides to change.
- **Distribution-typed fields on the wire.** `metrique_aggregation::Histogram<T>`, `SharedHistogram<T>`, and user distribution types lower to `FieldShape::Opaque` today. Dial9 skips them with a diagnostic when tagged `InTrace`. EMF/JSON render distributions normally. Evolution path is metrique-side: add `FieldShape::Distribution(KnownShape)` once `DescribeValue` lets aggregation types self-describe as distribution-shaped; dial9 then gains a corresponding wire variant as an additive change.
- **Nested containers beyond one level.** `Vec<Option<T>>` and `Flex<(String, Option<T>)>` are supported; deeper combinations (`Vec<Vec<T>>`, `Vec<Flex<..>>`, `Flex<(String, Vec<T>)>`, double-optional) lower to `FieldShape::Opaque` in metrique and are therefore skipped by dial9 when tagged `InTrace`. Widening the macro's recognition is additive on both sides: the metrique descriptor enum and dial9's `FieldType::List`/`FieldType::Map` wire variants already accept `&'static FieldShape` / `FieldType` recursively.

## Tradeoffs worth reviewer attention

- **Schema cache keyed on `&'static EntryDescriptor` pointer, not shape fingerprint.** This is correct because macro-derived descriptors are `'static` and unique per type. It works because the descriptor is the full closed-shape description; sinks never need to observe emissions to learn structure. Consequence: hand-written entries (which return `None` for the descriptor) are skipped, not encoded via a fingerprint fallback.
- **`no_write` introduces a new close-time behaviour users have to understand.** `ignore` and `no_write` are adjacent but distinct: `ignore` means "the macro pretends this field does not exist for metrics purposes"; `no_write` means "keep the field closed and retained, but do not emit through `EntryWriter`." The diagnostic story has to be careful to push users to the right one.
- **`InTrace` default inheritance interacts with `flatten`.** The rule we landed on (child explicit decisions win; parent defaults fill only unspecified) is the rule that lets `Dial9Context` protect its own fields from accidental `InTrace` inheritance when flattened. It is a rule reviewers should exercise against their own use cases.
- **Dial9 depends on a descriptor-system PR in metrique.** The dial9 implementation cannot land before the metrique PR does. The changelog doc tracks the specific dependency.
- **Units on the wire are schema annotations, not field-name suffixes.** This is a one-time downstream-tooling change: consumers looking for `latency_Microseconds` need to look at schema metadata instead. It is the right long-term shape; field-name suffix is a hack we did not want to permanently bake in.

## Key design choices

### Descriptor-aware sink, not runtime shape inference

The sink consumes a static descriptor emitted by metrique's macro. The descriptor covers all possible fields up front, including optionality and `Flex`, so the sink never needs to infer shape from observed emissions. One registration per entry type. Runtime fingerprinting was rejected as the primary mechanism for two reasons:

1. **Optional-field schema explosion.** A struct with K optional fields can produce up to 2^K distinct observed shapes under fingerprinting. Every observed shape registers a separate schema and re-emits the schema frame. The cache either grows unboundedly or thrashes.
2. **Unbounded Flex keys.** Each distinct `Flex::new(key)` value produces a distinct observed shape. For keys drawn from a high-cardinality source, the cache thrashes.

Fingerprinting is not on the roadmap as a fallback for hand-written entries. See "Hand-written entries" in the Non-goals list above.

### Context capture via a metrique source field, not a sink wrapper

Caller-thread context lives in a real metrique field (`Dial9Context`) whose constructor reads the tokio thread-locals and whose closed form is the snapshot the sink extracts via `desc.source::<Dial9>(..)`. An alternative design, rejected, would capture context through a sink wrapper that injects an `EntryConfig`.

Advantages:

- No sink wrapper in the composition path. Dial9 is a true peer sink.
- Capture runs in the entry's constructor, so context is recorded on the caller thread by construction, not by convention.
- The closed snapshot survives `BoxEntry` erasure in a typed way because it is reachable through `desc.source::<Dial9>(entry.inner_any())`.
- Users who want context visible as normal payload can `flatten` instead of `no_write`; the source data remains structurally available.

The sink wrapper is not precluded: users who want runtime-wide defaults can provide their own helper that constructs `Dial9Context` and merges it in. It is not the primary path.

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

### Validation strategy

Validation runs in three phases: compile-time intrinsic checks from the metrique macro, startup-time binary-wide discovery at sink construction, and per-descriptor first-use validation on the event path. Each catches a different class of error.

Compile-time checks are essentially free and entirely metrique's responsibility. They catch structural contradictions (duplicate source tags, conflicting field-tag attributes) but cannot catch dial9-specific rules because metrique treats tag identity opaquely.

Startup-time discovery uses metrique's `SourceTag::register_descriptor` hook. Dial9 implements the hook on its `Dial9` tag to push registered descriptors into a `Mutex<Vec<_>>`, and `Dial9Stream::new` inspects the vec at construction. The specific failure mode we want to catch: a user attaches a dial9 sink but no struct in the binary declares `source(Dial9)`. Without this check, the sink runs silently and produces no events, and the user finds out when they look at their trace file and see nothing.

The check is best-effort, not a correctness guarantee. Known false-positive and false-negative scenarios (multi-binary workspaces, feature-gated structs, deps shipping their own tagged entries) are enumerated in the keeper's Validation section. Users who hit a false negative disable the empty-registry warn per-sink via the builder (`Dial9Stream::builder(...).startup_discovery(false).build()`). On targets where link-time registration is unavailable, dial9's `SourceTag` override is cfg'd out automatically; no cargo feature is involved.

Per-descriptor first-use validation runs unconditionally, whether or not startup discovery ran. Every descriptor the sink sees on the event path gets walked once for dial9-specific structural errors (`InTrace` without `Dial9` source, `InternString` on non-string shape, opaque field tagged `InTrace`). The verdict caches on the `&'static EntryDescriptor` pointer.

Failure policy for dial9-specific structural errors: `debug_assert!` in debug, rate-limited `tracing::error!` in release. These are errors the user must fix; they do not have meaningful false positives. The broken entry is dropped from the dial9 path; EMF and other sinks are unaffected.

## Why not compile-time (in one place)

The compile-time path keeps coming up in review, so consolidating the argument here rather than scattering it across alternatives.

**Option 1: User derives `TraceEvent` on their metrique struct.** Blocked by `BoxEntry` erasure. The concrete type is gone by the time `Dial9Stream` sees the entry. Recovering it requires one of:

- A parallel object-safe trait plus a dial9-owned box type. Object safety itself is not the blocker (as Russell noted, `Encodeable` is `dyn`-safe). The blocker is threading the sink-specific bound through `BoxEntrySink::append_any`, which takes `impl Entry + Send + 'static` and boxes internally. That bound cannot be tightened without breaking every existing user.
- A metrique-side change so `ServiceMetrics` can attach a sink typed concretely over `TraceEvent + Entry`. This propagates the sink-specific bound through every composition primitive (`tee`, `BackgroundQueue`, `FixedFractionSample`, `CongressSample`, global attach). Any user combining two such sinks would have to satisfy both bounds simultaneously on every entry type. Ecosystem fragmentation is a concrete cost, not a slogan.
- A TypeId-keyed vtable bridge on `BoxEntry`, where each consumer registers a `TypeId -> &dyn T` table. This is architecturally what the descriptor is, just generalised across consumers instead of one table per sink. **The descriptor is the least-bad vtable bridge, done once for all consumers.**

**Option 2: Blanket `TraceField for impl Value`.** Russell's thread raised this. It works for the primitives path: every metrique primitive automatically gains a `TraceField` impl. It also works for user types that implement only `Value`. What it does not do is give the sink any compile-time shape information about those types; the blanket collapses to a dyn-dispatched `TraceField::encode` call. That is effectively runtime dispatch through a differently-named trait. The descriptor path already does that, with the benefit of also describing optionality, Flex, and units.

Without the blanket, users ship a bespoke `TraceField` impl per custom type, which is a maintenance cost that grows with the user's type inventory. With the blanket, compile-time shape is gone and we are back to runtime dispatch. Neither is better than reading the descriptor.

**Option 3: Metrique emits a per-sink compile-time wire plan.** This is the strongest compile-time path and we are not rejecting it forever. It would let dial9 skip `Entry::write` dispatch entirely on the flush thread: one `encode_for_dial9(entry, encoder)` function per entry type.

We are not building it now because:

- The descriptor carries strictly less information than a static plan would need. A plan layer can be added on top of the descriptor without breaking anything. The descriptor path is forward-compatible with this; it is not a permanent alternative.
- The dial9 cost on the flush thread is already off the caller's critical path (`BackgroundQueue` absorbs latency). The marginal win from a static plan is CPU on the flush thread, not latency on the caller thread.
- Shipping the plan requires metrique to either understand dial9's wire format (bad: tight coupling) or generate a sink-pluggable plan that dial9 consumes (more design work, not shorter than the descriptor design).

**Option 4: Hybrid. Descriptor for heterogeneous queues, typed sink for homogeneous pipelines.** This is real and we do not rule it out; a future typed-entry dial9 sink can coexist with the descriptor path. We chose not to ship two paths now because the descriptor covers both cases, and users wanting maximum performance on a homogeneous pipeline can add the static plan as a follow-up once the descriptor lands.

**Bottom line.** Compile-time would buy us flush-thread CPU and nothing else. It would cost us `BoxEntry` compatibility or ecosystem fragmentation, and it would still need a descriptor-or-equivalent to handle optional fields, Flex, units, and context extraction. The descriptor path is the one that survives after you cross all those items off.

## Alternatives considered

### Alternative A: Runtime shape fingerprinting (the original PR)

Rejected. See "Descriptor-aware sink, not runtime shape inference" above.

Kept as a bounded fallback for hand-written entries, then cut to "skip hand-written entries" to avoid two code paths. Can be reinstated if hand-written-entry support becomes important.

### Alternative B: Dial9-specific `#[derive(TraceEvent)]` on the same struct

Rejected. Two independent blockers.

1. **`BoxEntry` erasure.** `BoxEntrySink::append_any` takes `impl Entry + Send + 'static` and boxes internally. By the time `Dial9Stream` sees the entry, the concrete type (and any `TraceEvent` impl) has been erased. Preserving `TraceEvent`-ness requires either a parallel object-safe trait plus a dial9-owned box, or a metrique-side change so `ServiceMetrics` can attach a sink typed over `TraceEvent + Entry`. In Russell's review thread, he pointed out that `Encodeable` is `dyn`-safe; the blocker is not object safety but threading the type through `BoxEntrySink::append_any`, which does not carry that bound. The least-bad workaround is a TypeId-keyed vtable bridge on `BoxEntry`, which does not feel right.
2. **The `TraceField` blanket problem.** Every `Value` impl (primitives, unit wrappers, aggregation types) plus every user custom type would need a parallel `TraceField` impl unless a blanket `TraceField for impl Value` is provided. With the blanket, `TraceField` becomes runtime dispatch through `Value::write`, so the compile-time shape knowledge that motivated the path disappears. Without the blanket, each user type needs a hand-written `TraceField` impl forever. Either outcome is worse than reading a descriptor.

The descriptor path sidesteps both. It reads metrique's stable abstraction and does not require any sink-specific trait on `Value` impls.

### Alternative C: Compile-time dial9 wire plan

Rejected for this pass. A compile-time wire plan would let dial9 skip `Entry::write` dispatch on the flush thread. It would be strictly better for performance, but it does not unlock any of the functional requirements; the descriptor path already meets them.

Static plans are an evolution path, not a prerequisite. The descriptor already carries strictly less information than a static plan would; adding a plan layer on top is additive.

### Alternative D: `D9Meta` / `Dial9Meta` with a `Default` impl and flatten-only sugar

Russell's proposal: users write `#[metrics(flatten)] d9: D9Meta` with `..Default::default()` on construction, and the sink picks D9Meta out of the flattened fields.

Rejected as the primary path because:

- It conflates source semantics with field emission. Context data does not always belong in normal emission.
- The sink identifying "this is the dial9 context" by convention is fragile. A typed `SourceTag`-driven extractor accessed via `desc.source::<Dial9>(..)` is a better contract.
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

### Alternative L: User-invoked compile-time validation macro

Proposed shape: dial9 ships `dial9::assert_dial9_compatible!(MyStruct)`. Users place it next to their `#[metrics(..)]` struct; the macro runs compile-time checks on the descriptor.

Rejected. An opt-in compile-time check is a runtime check for anyone who does not remember to invoke it, which is most users most of the time. It makes the "compile-time" label dishonest. The check itself runs in every sink path at runtime, so the value added by the opt-in is narrow. Dropping it keeps the story clean: compile-time for what the macro can see intrinsically; runtime for everything sink-specific.

### Alternative M: Grace-period / first-entry traffic sampling

Proposed shape: after sink construction, if the sink has processed no dial9-bearing entries within N seconds (or within the first N entries), emit a warn.

Rejected. Indistinguishable from normal idle or startup behaviour. A pipeline that happens to run cold for a period is not misconfigured, but the check cannot tell. Pre-traffic configuration errors should be caught pre-traffic or not signalled this way.

### Alternative N: Tuple-based type-list at sink construction

Proposed shape: `Dial9Stream::builder().expect_compatible::<(T1, T2, ...)>().build()`. Users list the entry types they expect to flow through, and the builder validates them at const-eval time.

Rejected. Reintroduces user ceremony at the sink construction site, which is exactly the kind of boilerplate the design aims to eliminate. Users who forget to list a type get no diagnostic for it; users who list a type they no longer use get a stale type list. The `SourceTag` hook discovers the set automatically.

### Alternative O: `linkme` (or similar) in metrique's public API

Proposed shape: metrique's `SourceTag` trait exposes an associated `linkme::DistributedSlice` that sinks point at their own slice.

Rejected. Pins metrique's public API to a specific link-time mechanism. If `linkme` needs to be swapped for a future mechanism (stable distributed slices, a different crate, a cfg-gated alternative), every sink has to migrate. The trait-method form (`fn register_descriptor(&'static EntryDescriptor)`) keeps the mechanism entirely private to metrique's macro expansion and each sink's own impl; swapping is an internal refactor.

### Alternative P: Imperative pre-main registration via `ctor` exposed to users

Proposed shape: metrique emits `#[ctor::ctor] fn` wrappers that users would see if they read macro-expanded output; the pre-main side effect is documented as part of the contract.

Rejected as a user-facing concept. Users should not have to reason about pre-main code execution to understand their entry types. The pre-main registration, whether via `ctor`, `linkme`, or something else, is an implementation detail behind the `SourceTag::register_descriptor` hook. The keeper doc describes the hook; the impl plan pins the specific mechanism.

### Alternative Q: Cargo feature for startup-discovery opt-out

A shape we rejected: ship a `no_startup_discovery` (or inverted `startup_discovery`) cargo feature on the dial9 crate that gates dial9's `SourceTag::register_descriptor` override, the `linkme` dependency, and the empty-registry check.

Rejected because cargo features are infectious: a workspace using dial9 via multiple dependencies gets the union of all feature sets, so one crate enabling the default features forces every crate in the graph to do the same. We need two different opt-outs for two different problems: build-time impossibility on targets where `linkme` does not work, and runtime false-positive suppression per-sink. The current design handles them separately: the build-time case is a target-cfg gate on dial9's `SourceTag` override (no cargo feature involved, automatic per-target); the runtime case is a per-sink `.startup_discovery(false)` builder toggle. Neither carries the cargo-feature infection risk.

## Feasibility checks

- `BoxEntry::inner()` returns `&(dyn Any + Send + 'static)`; the concrete closed entry is reachable for typed source extraction via `desc.source::<Dial9>(..)`.
- Adding one method (`descriptor()`) to the erased entry trait is a metrique-side surface change, not a dial9-side one. Dial9 depends on it; see the impl plan for sequencing.
- `tee` and `BackgroundQueue` are public; the existing composition paths continue to work unchanged.
- `EntryConfig` is retained; it is the right primitive for per-emission, sink-provided data. Descriptors and sources cover per-type, entry-provided data. The two coexist.
- `dial9_tokio_telemetry::telemetry::clock_monotonic_ns()` is `pub` and callable from any thread; `Dial9Context::capture()` uses it directly.
- Schema annotations and typed dynamic maps are additive extensions of `dial9-trace-format`. We do not bump the format version. Old decoders hitting an unknown top-level frame tag or an unknown `FieldType` variant return `None` and halt; new traces silently truncate at the first extension when read with an older viewer. We accept that behaviour because the format is not widely distributed outside this repo, the viewer ships in-tree, and users producing new traces are in the same update cycle as the viewer. A version bump was considered and rejected as unnecessary cost; adding the behaviour here so reviewers can flag if they disagree.
d as unnecessary cost; adding the behaviour here so reviewers can flag if they disagree.
