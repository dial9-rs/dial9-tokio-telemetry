# Metrique integration: implementation plan

**This document is deleted as part of PR sign-off. It captures implementation sequencing, module-level tweaks, and the intentional scope limits we plan to follow up on.**

Candid status disclaimer: as of this PR, nothing here is implemented. The dial9 side depends on a metrique PR that is not yet open. This plan exists so reviewers can evaluate whether the scope and sequencing are sound.

## Sequencing

1. **Metrique PR lands first.** Adds `EntryDescriptor`, `Source<C>`, `SourceTag`, field-tag attributes, `no_emit`, and the `descriptor()` hook on the erased entry vtable. See `metrique/docs/entry-descriptors.md`. Stub: https://github.com/awslabs/metrique/pulls (to be opened).
2. **Metrique releases a version carrying those APIs.** Dial9 pins to that version.
3. **Dial9 trace-format additions land.** Schema annotations, typed dynamic map wire type.
4. **Dial9 sink implementation.** `Dial9Context`, `Dial9Stream`, the `Dial9EntryWriter` adapter, `attach_to_stream_with_dial9` / `metrique_sink` / manual primitives refreshed for the new capture path.
5. **Integration tests and docs.** End-to-end test with a representative user struct. Update the dial9 README with the recommended usage shape.

Steps 3 and 4 can proceed in parallel once step 2 is done. Step 5 is the last gate.

The PR that accompanies this design doc implements none of the above; it is design only. The implementation PRs will consume the descriptor APIs as specified here and will not land until the metrique side is released.

## Metrique-side work (tracked separately, but required for dial9)

High-level sketch of what the metrique PR has to touch. Full design is in the metrique repo.

- **`metrique-macro/src/lib.rs`**: accept new attributes (`default_field_tag`, `field_tag`, `source`, `no_emit`), reject misuse at macro-expansion time.
- **`metrique-macro/src/structs.rs`**: generate the `static EntryDescriptor` constant and `impl Source<C>` blocks; wire `no_emit` fields so they close and are retained but are not emitted via `EntryWriter`.
- **`metrique-macro/src/entry_impl.rs`**: ensure generated `Entry::write` order matches the descriptor field order; omit `no_emit` fields from the write path.
- **`metrique-macro/src/value_impl.rs`**: no change required for v1; future compile-time value-shape work lives here.
- **`metrique-core`** (erased entry): add `descriptor(&self) -> Option<&'static EntryDescriptor>` to the object-safe dyn-trait backing `BoxEntry`. Default impl returns `None`.
- **`metrique-writer-core`**: define `EntryDescriptor`, `FieldDescriptor`, `FieldShape`, `SourceDescriptor`, `SourceExtractor`, `Source<C>` in a public module (exact location TBD in the metrique PR; `metrique-writer-core::descriptor` is likely).
- **`metrique-writer-core::unit`**: surface `Unit` on `FieldDescriptor`; keep existing behaviour where the macro reads `#[metrics(unit = ...)]`.

The metrique PR is also on the hook for:

- Macro-level static diagnostics (duplicate source tag, conflicting `field_tag` + `field_tag(skip)`, etc.).
- Policy decisions around `no_emit` (see scope limits below).
- Documentation updates to the metrique README or user guide.

## Dial9 trace-format additions

Package: `dial9-trace-format`.

### Schema-level annotations

- New type: `FieldAnnotation { field_index: u16, key: String, value: String }`.
- Extend `SchemaEntry` with `annotations: Vec<FieldAnnotation>` (or a parallel section if we want to avoid bumping every existing schema's wire size with an empty vec).
- Extend the schema wire format with an optional annotations section. Back-compat: older readers that do not know about annotations must continue to decode the schema; either reserve a tag now or bump a format version.
- Expose a builder or constructor path that lets the encoder attach annotations at registration time.

The first consumer is units, written as `("metrique.unit", "<unit name>")`. No assumption that the value string has a fixed grammar; `Unit::Custom("widgets/request")` is allowed.

### Typed dynamic maps

- New `FieldType` variants for fixed-schema key-string maps: `StringMap<V>` family. Candidate tags: `StringMapI64`, `StringMapU64`, `StringMapF64`, `StringMapString`, `StringMapPooledString`. Wire encoding: `<count> <repeated (key, value)>` with key encoded per the map's key type and value encoded per `V`.
- Optional variant of each (the existing high-bit convention).
- Decoder support for all new variants, including `FieldValue` / `FieldValueRef` variants and `StringMapIter` extensions.

If the full type matrix becomes unwieldy, we can fall back to a single `Map { key: FieldType, value: FieldType }` shape decoded by walking key/value pairs, at the cost of one more byte per event. Decide during implementation.

## Dial9 sink implementation

Package: `dial9-tokio-telemetry`.

- **`src/metrique/context.rs`** (new): `Dial9Context` metrique field type. `#[metrics(source(Dial9))]` and `#[metrics(default_field_tag(skip(InTrace)))]` at the struct level. `capture()` reads worker/task/monotonic. Closed form holds both caller-thread and flush-thread snapshots as needed.
- **`src/metrique/tags.rs`** (new): `pub struct Dial9;`, `pub struct InTrace;`, `pub struct InternString;`. These are the tag types users reference from their own structs.
- **`src/metrique/stream.rs`** (was `dial9_stream.rs` if extracted in round 1): `Dial9Stream` reworked as a descriptor-aware sink. Fast-path checks (no descriptor → skip; no `InTrace` fields → drop cheaply; present `InTrace` fields but no `Dial9` source → report).
- **`src/metrique/writer.rs`** (new): `Dial9EntryWriter` adapter. Walks `Entry::write` callbacks, cross-references the descriptor to filter by `InTrace`, routes `InternString` fields through `encoder.intern_string`, and encodes each value according to its `FieldShape`.
- **`src/metrique/schema.rs`** (new): builds `SchemaEntry` (with annotations) from an `EntryDescriptor`. Caches on the `&'static EntryDescriptor` pointer.
- **`src/metrique/builder.rs`**: refresh to drop `TokioContextSink` from the default composition. Keep the public helper types exported for manual users, but stop requiring them for correctness.
- **`src/metrique/mod.rs`**: top-level `metrique_sink(...)`, `AttachDial9Ext::attach_to_stream_with_dial9`, public re-exports of `Dial9Context`, `Dial9`, `InTrace`, `InternString`.

## New public APIs at the boundary

### In metrique (for reference; designed in the metrique repo)

```rust
// metrique-writer-core / metrique re-exports
pub struct EntryDescriptor { /* ... */ }
pub struct FieldDescriptor { /* ... */ }
pub enum FieldShape { /* ... */ }
pub struct SourceDescriptor { /* ... */ }
pub struct SourceExtractor { /* ... */ }
pub trait Source<C> { type Snapshot; fn snapshot(&self) -> Self::Snapshot; }

// Erased entry trait (method added to the existing dyn trait object behind BoxEntry)
fn descriptor(&self) -> Option<&'static EntryDescriptor>;

// Macro attributes
#[metrics(default_field_tag(T))]
#[metrics(default_field_tag(skip(T)))]
#[metrics(field_tag(T))]
#[metrics(field_tag(skip(T)))]
#[metrics(source(T))]
#[metrics(no_emit)]
```

### In dial9

```rust
pub struct Dial9;
pub struct InTrace;
pub struct InternString;

#[metrics(source(Dial9))]
#[metrics(default_field_tag(skip(InTrace)))]
pub struct Dial9Context { /* private */ }

impl Dial9Context {
    pub fn capture() -> Self;
}

pub struct Dial9Stream { /* ... */ }
impl Dial9Stream {
    pub fn new(handle: &TelemetryHandle) -> Self;
}

pub fn metrique_sink(
    inner: impl EntryIoStream,
    handle: &TelemetryHandle,
) -> MetriqueSinkBuilder;

pub trait AttachDial9Ext {
    fn attach_to_stream_with_dial9(
        stream: impl EntryIoStream,
        handle: &TelemetryHandle,
    ) -> AttachHandle;
}
```

Exact signatures may shift during implementation; this is the shape reviewers are agreeing to at the design level.

## Intentional scope limits

Items consciously left out of this round. Each has a clear follow-up path; none of them are blockers for the core design.

- **Hand-written `Entry` impls return `None` from `descriptor()` by default**. Users with `impl Entry for MyType` (no `#[metrics]`) keep working on format-level sinks (EMF, JSON) and are skipped by descriptor-aware sinks. Follow-up: once metrique ships `DescribeEntry`, dial9 picks up hand-written users with no change on the dial9 side. See the dial9 review doc's "Hand-written entries and manual dial9 opt-in" section.
- **No runtime `Entry::write` fingerprinter as a hand-written-entry fallback.** Decision, not deferral: the fingerprinter would recreate the optional-field and Flex explosion problems the descriptor path eliminates. If hand-written demand is real and `DescribeEntry` takes longer than expected, we revisit.
- **Only one source per tag per entry**. Multiple sources for the same tag are rejected. Follow-up when a use case appears.
- **No optional sources**. An entry either has a source for tag `T` or it does not. Follow-up when a use case appears.
- **`FieldShape::Opaque` fields selected for `InTrace` are reported and skipped**. We do not add a runtime "encode unknown as string" path. Follow-up: either the user adds a known value type, or a future extension defines a tagged dynamic value.
- **Flex values are homogeneous (one `T` per map)**. Heterogeneous dynamic maps (`map<string, Any>`) are out of scope and would need both a metrique value-tag model and a dial9 tagged wire value.
- **Compile-time dial9 wire plan.** Not implemented. The descriptor path is enough; a static plan is strictly additive on top.
- **No user-invoked compile-time validation helper.** An earlier draft proposed `assert_dial9_compatible!(T)`; dropped. Opt-in compile-time checks are functionally runtime checks for anyone who forgets to invoke them, and the checks already run at first-use and at startup-time discovery. See rejected Alternative L in the review doc.
- **Programmatic `Dial9StatsHandle`.** Not implemented. Diagnosis uses periodic `tracing::debug!` and rate-limited `tracing::warn!`. Follow-up once metrique exposes a richer reporting hook (see [metrique#205](https://github.com/awslabs/metrique/issues/205)).
- **Schema-cache tunability.** The cache is keyed on `&'static` pointers, so its size is a compile-time property. No public tuning surface needed.
- **Format-layer sampling integration.** `FixedFractionSample` and `CongressSample` wrap a `Format`, not an `EntryIoStream`. Dial9 stays an `EntryIoStream`; sampling integration is a follow-up that needs either a new composition shape or a metrique-side change.
- **Structured display hints / privacy labels on the wire.** The schema-annotation mechanism supports these; no consumer besides units in this round.

## Startup-time discovery: linkme, platform support, and the opt-out feature

Metrique's `SourceTag::register_descriptor` hook is the mechanism dial9 uses for binary-wide discovery. Concrete pins:

- **Registration mechanism: `linkme` 0.3.x**, pulled in as a metrique-internal dependency to drive the registration call, plus a dial9-internal dependency to populate dial9's own per-source registry. Not exposed in metrique's public API; the public contract is the `SourceTag` trait.
- **Why `linkme` over `ctor` or `inventory`**: `linkme` is data-driven (registrations are `#[link_section]` statics grouped by the linker) so no code runs pre-main. `ctor` runs a function per registration pre-main, which is slightly heavier and harder to debug. `linkme`'s failure mode (empty slice) is the same failure mode we already handle for "no entries registered."
- **Platform coverage**: `linkme` works on Linux, macOS, Windows, FreeBSD, NetBSD, OpenBSD, Android. WASM requires a feature flag and is not part of dial9's target matrix. `no_std` is out of scope for this work.
- **Known platform gotchas**:
  - `cdylib` or `staticlib` users linking dial9 into a larger application may see empty registrations unless they pass `-Wl,--whole-archive` (or the platform equivalent) on the dial9 archive. Document in the dial9 README near the sink setup instructions.
  - `cargo test` test binaries are separate from production binaries; registrations from tests do not leak. Registrations from the library under test are included because the test binary links the same `rlib`.
- **`no_startup_discovery` feature on the `dial9-tokio-telemetry` crate**: feature-gates the entire startup-time discovery path. When set:
  - Dial9 does not depend on `linkme` (via cargo optional-dependencies).
  - `impl metrique::SourceTag for Dial9` uses the defaulted no-op `register_descriptor`.
  - `Dial9Stream::new` does not iterate a registry and does not emit the empty-registry warn.
  - Per-descriptor first-use validation continues to run on the event path.
- **Release-vs-debug policy**:
  - First-use descriptor-local checks (InTrace-without-Dial9-source, InternString-on-non-string, Opaque-in-InTrace): `debug_assert!` panic in debug, rate-limited `tracing::error!` in release. No opt-out; these describe clearly-wrong descriptors.
  - Empty-registry warn: `tracing::warn!` in all builds. `no_startup_discovery` is the opt-out.

## Risks and mitigations

- **Metrique PR scope.** The descriptor work is non-trivial. Mitigation: the metrique design doc is explicit about what's in scope; metrique reviewers can veto pieces before dial9 starts consuming. Dial9 side can be scoped down if metrique ships a narrower initial API (e.g. no descriptor for enum entries).
- **Format-version compatibility.** Adding schema annotations and new `FieldType` variants changes the wire format. Mitigation: introduce under a version bump so older readers reject the file at the header rather than silently truncating. The existing format's pre-main reserved-tag space is too narrow for safe in-place extension; committing to `VERSION = 2` gives room for both annotations and the typed dynamic-map field types in one go.
- **Caller-thread cost regression.** `Dial9Context::capture()` does roughly the same work `TokioContextSink` did (a few TL reads plus `clock_monotonic_ns()`). We expect no measurable regression, but will benchmark during implementation.
- **Hand-written-entry users surprised by silent skip.** Mitigation: rate-limited `tracing::warn!` the first time a hand-written entry is observed, keyed on the concrete type id. Document that hand-written entries are not yet supported without `DescribeEntry`.
- **`linkme` platform or linker-configuration problems**. Mitigation: `no_startup_discovery` opt-out feature as described above; document the `-Wl,--whole-archive` gotcha.

## Testing plan

- Trace-format unit tests for schema annotations and typed dynamic maps, including backward-compat decoding through the version bump.
- Descriptor round-trip: a user-space struct with optionals, Flex, units, and tags; assert the sink registers exactly one schema with the expected annotations.
- Context extraction: a caller-thread test that captures `Dial9Context`, passes through `BackgroundQueue`, and verifies the flush-thread snapshot matches.
- Heterogeneous queue: a `BoxEntrySink<BoxEntry>` with multiple struct types, confirming each gets one schema and one source extraction.
- Startup-time discovery: a binary with one `source(Dial9)` struct and a binary with none, asserting the empty-registry warn fires in the second case only. Feature-gated parallel run with `no_startup_discovery` verifies the warn is suppressed.
- Per-descriptor first-use validation: construct descriptors that violate each check (InTrace-without-Dial9-source, InternString-on-non-string, Opaque-in-InTrace), assert `debug_assert!` panic in debug and rate-limited `tracing::error!` in release.
- Disabled-handle / no-source / no-`InTrace` paths: assert no-op behaviour and correct report rates.
- Panic isolation: a `Value::write` that panics, asserting the offending event drops without poisoning the flush thread.
- End-to-end: a representative example in `examples/` producing a trace file that the viewer can render, including both tokio runtime events and metrique-originated events.
end: a representative example in `examples/` producing a trace file that the viewer can render, including both tokio runtime events and metrique-originated events.
