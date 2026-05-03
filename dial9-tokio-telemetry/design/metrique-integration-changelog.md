# Metrique integration: changelog (round 1 → round 2)

**This document is deleted as part of PR sign-off. Keeper docs are `metrique-integration.md` and (indirectly) `docs/entry-descriptors.md` in the metrique repo.**

This summarises what changed in the design since the first round of review on PR #346 and why.

## Headline change

Dial9 stops doing runtime schema discovery. Metrique gains a static entry-descriptor system, a source (provider) system for structural context, and a field-tag system for per-sink opt-in. Dial9 becomes a descriptor-aware sink.

The round-1 design was functional but could not structurally handle optional-field schema explosion or unbounded Flex keys. It also captured caller-thread context through a sink-wrapper that made dial9 a privileged sink in the composition.

## What changed, by area

### Schema discovery

Round 1: `Dial9Stream` walked `Entry::write` on every event, hashed `(name, field_type)` pairs into a shape fingerprint, and looked up or registered a dial9 schema in a bounded LRU cache.

Round 2: metrique emits a `&'static EntryDescriptor` per macro-derived entry. `Dial9Stream` keys its schema cache on the descriptor pointer. No fingerprinting, no LRU eviction, no thrash path. Hand-written entries return `None` for the descriptor and are skipped with a rate-limited report.

### Optional fields

Round 1: each observed optional combination registered a separate schema (up to 2^K for K optionals). Bounded LRU kept cardinality in check for typical structs but broke down for structs with several optionals.

Round 2: the descriptor marks optional fields structurally (`FieldShape::Optional`). One schema per entry type, regardless of which optionals happen to be present at runtime.

### Flex

Round 1: each distinct Flex key registered a new schema. Bounded LRU handled bounded keys but was a thrash risk for high-cardinality keys.

Round 2: Flex lowers to `FieldShape::Flex { key, value }` in the descriptor and to a new typed dynamic-map wire type in `dial9-trace-format`. One schema per Flex-bearing entry type, regardless of runtime keys. An `InternString` field tag opts the keys into the dial9 string pool.

### Caller-thread context

Round 1: `TokioContextSink` wrapped the outer sink and injected an `EntryConfig` carrying the captured context. Dial9 was effectively privileged in the composition.

Round 2: context lives in a `Dial9Context` metrique field. The field's constructor captures caller-thread state; its closed form (`ClosedDial9Context`) is the snapshot the sink extracts through `Source<Dial9>`. Dial9 returns to being a peer sink. `TokioContextSink` is no longer required.

### Per-field opt-in

Round 1: every field emitted to EMF also emitted to dial9. No granular control. Russell flagged this as a real problem: users often want just a request id and a KPI or two in the dial9 view, not the whole wide event.

Round 2: dial9 defines `InTrace` as a field tag. Users opt fields in with `default_field_tag(InTrace)` at the struct level, or with `field_tag(InTrace)` at the field level, and opt out with the matching `skip(...)` form.

### String interning

Round 1: dial9 did not expose string interning to the metrique path.

Round 2: `InternString` is a separate field tag. It is orthogonal to `InTrace`; it says "if this emitted field carries string data, route it through dial9's string pool." Applies uniformly, including Flex keys and values when explicitly requested.

### Units

Round 1: units were stamped into field names (`latency_Microseconds`). Ugly and not round-trippable; `Unit::Custom` produced awkward suffixes.

Round 2: units stay first-class in the descriptor (`Option<Unit>` per field). Dial9 emits them as schema-level annotations (`("metrique.unit", "microseconds")`). Fields with no unit cost zero bytes. This generalises to other per-field metadata (display hints, privacy, semantic conventions) without further format churn.

### Heterogeneous queues / `BoxEntry` erasure

Round 1: descriptor lookup not a problem in round 1 because there was no descriptor; instead the problem was that `TraceEvent`-ness could not survive `BoxEntry` erasure.

Round 2: descriptor lookup goes through the erased entry vtable via a new method that returns `Option<&'static EntryDescriptor>`. `BoxEntry` size is unchanged. Descriptor-unaware sinks never call the method.

### User API (sink composition)

Mostly unchanged. Global (`attach_to_stream_with_dial9`), builder (`metrique_sink(...)`), and manual (`tee(emf, Dial9Stream::new(...))`) paths stay. The manual path no longer requires `TokioContextSink`.

### User API (entry definition)

New: `#[metrics(default_field_tag(...))]`, `#[metrics(field_tag(...))]`, `#[metrics(source(...))]`, `#[metrics(no_emit)]`. `skip(T)` is an argument form of the two tag attributes. These are general metrique features, not dial9-specific.

Round-1 entries are unchanged but do not produce dial9 traces. Users opt in by adding the tags and the `Dial9Context` field.

### Validation

Round 1: no dial9-specific validation story. Misuse was caught if at all by the fact that the runtime-fingerprinted schema looked wrong.

Round 2: three-tier validation.

- Compile-time, in the metrique macro: intrinsic structural checks (duplicate sources, conflicting tags). Independent of any sink.
- Startup-time, at `Dial9Stream::new`: dial9 registers every descriptor declaring `source(Dial9)` via metrique's `SourceTag::register_descriptor` hook (backed by `linkme` internally). Empty registry when a dial9 sink is constructed produces a `tracing::warn!`. Users who hit legitimate false negatives disable via `no_startup_discovery`.
- First-use, per descriptor: dial9-specific structural checks (InTrace without Dial9 source, InternString on non-string, Opaque in InTrace) run once per descriptor on the event path. `debug_assert!` in debug, rate-limited `tracing::error!` in release.

## Dependency on metrique

Round 2 depends on a metrique PR that adds the descriptor system, source system (including the `SourceTag` trait with a `register_descriptor` hook), field-tag attributes, and a `descriptor()` method on the erased entry vtable. The dial9 PR cannot merge before the metrique PR lands on a released version.

Linked references:

- metrique entry-descriptor design: `docs/entry-descriptors.md` (to be added in the metrique PR)
- metrique PR: TBD, stub link: https://github.com/awslabs/metrique/pulls

## Requirements: what changed

Round 1 targeted requirements stayed. Round 2 adds explicit requirements that round 1 did not fully meet:

- Per-field opt-in at struct and field granularity.
- Optional-field schema stability (one schema regardless of optional combinations).
- Flex schema stability (one schema regardless of runtime keys).
- First-class units on the wire without field-name mangling.
- Entry-owned context capture, with dial9 as a peer sink.

## Alternatives that stayed rejected

- Compile-time `#[derive(TraceEvent)]` on metrique structs: rejected for the same reasons (BoxEntry erasure + parallel TraceField maintenance).
- Dial9 as a pure `Format`: rejected (capture timing).
- Wrapping metrique composition primitives: rejected (fragments ecosystem).
- Separate dial9-owned background thread: rejected (duplicates BackgroundQueue).
- Programmatic stats handle in v1: still deferred.

## Alternatives that became the new baseline

- Static entry descriptor with explicit `FieldShape::Optional` / `Flex` entries (new in round 2).
- `Source<C>` extraction for caller-thread context (new in round 2).
- `InTrace`/`InternString` field tags (new in round 2).

## Open questions flagged in round-1 review, resolved

- Russell: "does this handle optional fields?" → yes, structurally, via `FieldShape::Optional`.
- Russell: "what we really want is 'tell me all the fields you can possibly produce'" → that is exactly what the descriptor does.
- Russell: "we may want an opt-in flag to make fields part of the dial9 record" → `InTrace` field tag.
- Russell: "we need to ensure there is a reasonable way to record context when the event starts" → `Dial9Context` source field.
- Russell: "we need to consider units" → schema-level annotation, no per-event overhead.
- Russell: "add a flag to flex so callers know it's flexing" → `FieldShape::Flex` in the descriptor; typed dynamic map on the wire.
- Jess: "ok with runtime interpretation and heavier encoding on the runtime thread if we can meet the requirements?" → Round 2 moves the heavy lifting to metrique-compile-time descriptors; flush-thread work shrinks compared to round 1.
