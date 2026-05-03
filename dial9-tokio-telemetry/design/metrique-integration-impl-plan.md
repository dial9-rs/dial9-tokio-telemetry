# Metrique integration: implementation plan

**This document is deleted as part of PR sign-off. It captures what the implementation work looks like, in what order, in which files, and which design decisions each piece depends on.**

Status: as of this PR, nothing here is implemented. The dial9 work depends on a not-yet-opened metrique PR. This plan exists so reviewers can evaluate whether the scope and sequencing are sound.

## Sequencing

Work graph is three tracks with explicit dependencies. Tracks run in parallel where the graph permits.

### Track A: metrique descriptor + source system

Prerequisite for everything downstream. Lives in a separate PR on the metrique repo.

- A1. Define `EntryDescriptor`, `FieldDescriptor`, `FieldShape`, `SourceDescriptor`, `SourceExtractor`, `Source<C>`, and `SourceTag` in `metrique-writer-core`. Ties to: metrique keeper's "The descriptor model" and "Sources and extractors" sections.
- A2. Add `descriptor(&self) -> Option<&'static EntryDescriptor>` to the object-safe dyn-trait backing `BoxEntry`. Default impl returns `None`. Ties to: metrique keeper's "Descriptor lookup" section.
- A3. Accept new attributes in `metrique-macro`: `default_field_tag`, `field_tag` (with `skip(T)` argument form), `source(T)`, `no_emit`. Ties to: metrique keeper's "Field tags", "Sources and extractors", and "`no_emit`" sections.
- A4. Generate, from `metrique-macro`, the `static EntryDescriptor`, the per-source `impl Source<C>` blocks, and the per-source link-time registration that invokes `<T as SourceTag>::register_descriptor`. Ties to: metrique keeper's "Descriptor lookup" and "Sources and extractors" sections; review's "Startup-time discovery mechanism" section.
- A5. Surface `Unit` on `FieldDescriptor`. Ties to: metrique keeper's "Units" section and dial9 keeper's "Units" section.
- A6. Macro-level static diagnostics for the validation-catalogue items that the macro can catch: duplicate sources, conflicting tag attributes, `SourceTag` trait-bound failures. Ties to: metrique keeper's "Validation → Compile-time" section.
- A7. Documentation: update the metrique README and crate docs with the new attribute set and the `SourceTag` contract.

Parallelism within Track A: A1 is a prerequisite for A2-A7. A2, A3, and A5 can proceed in parallel once A1 is stable. A4 depends on A1-A3. A6 depends on A3. A7 can follow rolling as other items stabilise.

Track A ships as its own metrique release. Dial9 pins to that release.

### Track B: dial9 trace-format additions

Depends on nothing in Track A. Can start as soon as this design is approved.

- B1. Bump `dial9-trace-format` wire format to `VERSION = 2`. Ties to: dial9 keeper's "Trace format additions" section.
- B2. Add schema-level annotations: `FieldAnnotation { field_index, key, value }`, new annotation section in the schema frame, encoder/decoder support. Ties to: dial9 keeper's "Units" section.
- B3. Add typed dynamic map `FieldType` support. Initial implementation uses a single `Map { key: FieldType, value: FieldType }` variant decoded by walking key/value pairs. Covers the full metrique Flex shape in one variant rather than a per-type matrix. Ties to: dial9 keeper's "Flex" section.
- B4. Update decoder / `FieldValue` / `FieldValueRef` for the new variants.
- B5. Update the dial9 viewer to render annotations and typed maps sensibly.
- B6. Regenerate the demo trace once the format is settled.

Parallelism within Track B: B1-B4 proceed together; B5-B6 come after B1-B4 stabilise.

### Track C: dial9 sink implementation

Depends on Track A (released metrique) and Track B (released trace-format).

- C1. `src/metrique/tags.rs`: `pub struct Dial9;`, `pub struct InTrace;`, `pub struct InternString;`. `impl metrique::SourceTag for Dial9` pushes descriptors into a `Mutex<Vec<&'static EntryDescriptor>>` via `linkme`. Ties to: dial9 keeper's "User-facing API" section; review's "Validation strategy" section.
- C2. `src/metrique/context.rs`: `Dial9Context` metrique field type with `#[metrics(source(Dial9))]` and `#[metrics(default_field_tag(skip(InTrace)))]`. `capture()` reads worker/task/monotonic. Closed form holds caller-thread + flush-thread snapshot. Ties to: dial9 keeper's "Components → `Dial9Context`" section.
- C3. `src/metrique/schema.rs`: `SchemaEntry` builder that takes an `EntryDescriptor` and produces the wire schema with annotations. Keyed cache on the `&'static EntryDescriptor` pointer. Ties to: dial9 keeper's "Components → Schema handling" section.
- C4. `src/metrique/writer.rs`: `Dial9EntryWriter` adapter walking `Entry::write` callbacks, cross-referencing the descriptor to filter by `InTrace`, routing `InternString` fields through the string pool, encoding per `FieldShape`. Ties to: dial9 keeper's architecture diagram "inside Dial9Stream" block.
- C5. `src/metrique/stream.rs`: `Dial9Stream` implements `EntryIoStream::next`. Descriptor-aware fast paths; per-descriptor first-use validation; `Dial9Stream::new` inspects the startup registry from C1 and emits the empty-registry warn. Ties to: dial9 keeper's "Validation → Startup-time" and "First-use" sections.
- C6. `src/metrique/builder.rs` + `src/metrique/mod.rs`: the three composition paths (global `attach_to_stream_with_dial9`, builder `metrique_sink`, manual `tee(emf, Dial9Stream::new(...))`). The manual path does not wrap the outer sink; caller-thread context capture is entirely in `Dial9Context`. Ties to: dial9 keeper's "User-facing API → Sink construction" section.
- C7. Feature flag wiring: `no_startup_discovery` opt-out, including cargo optional-dependency on `linkme`. Ties to: dial9 keeper's "Validation → Startup-time" section; impl plan's "Startup-time discovery" section below.
- C8. Documentation: dial9 README section on the new API shape, `Dial9Context` usage, the `-Wl,--whole-archive` footnote for `cdylib` users.

Parallelism within Track C: C1-C5 can start concurrently once Track A is released. C6 depends on C1 and C5. C7 touches C1 and C5 (feature gating). C8 lags everything else.

### Testing (Track D)

Tests are authored alongside the code in each track.

- D1. Trace-format unit tests for schema annotations and typed dynamic maps, including cross-version decode (VERSION 1 vs 2). In Track B.
- D2. Descriptor round-trip: a user-space struct with optionals, Flex, units, and tags; assert the sink registers exactly one schema with the expected annotations. In Track C.
- D3. Caller-thread context extraction: caller captures `Dial9Context`, passes through `BackgroundQueue`, flush-thread snapshot matches. In Track C.
- D4. Heterogeneous queue: a `BoxEntrySink<BoxEntry>` with multiple struct types, each gets one schema and one source extraction. In Track C.
- D5. Startup-time discovery: a binary with one `source(Dial9)` struct and a binary with none, assert the empty-registry warn fires in the second case only. Parallel run with `no_startup_discovery` feature verifies the warn is suppressed. In Track C.
- D6. Per-descriptor first-use validation: descriptors violating each check trigger `debug_assert!` in debug and rate-limited `tracing::error!` in release. In Track C.
- D7. Panic isolation: a `Value::write` that panics drops the offending event without poisoning the flush thread. In Track C.
- D8. End-to-end example in `examples/` producing a viewable trace with both runtime and metrique events. Last gate.

## Startup-time discovery: linkme, platform support, feature flag

`linkme` 0.3.x is the registration mechanism. Metrique uses it internally to drive the `SourceTag::register_descriptor` call per declared source; dial9 uses it directly to populate its own registry.

- `linkme` is not in metrique's public API. Metrique's public contract is the `SourceTag` trait; the mechanism behind the hook is an implementation detail and can swap without an API break.
- Platform coverage: Linux, macOS, Windows, FreeBSD, NetBSD, OpenBSD, Android. WASM requires a feature flag and is not in dial9's target matrix. `no_std` is out of scope.
- Known gotchas:
  - `cdylib` or `staticlib` users linking dial9 into a larger application may see empty registrations unless they pass `-Wl,--whole-archive` (or the platform equivalent) on the dial9 archive. Documented in the dial9 README.
  - `cargo test` test binaries are separate from production binaries; registrations from tests do not leak. Registrations from the library under test are included because the test binary links the same `rlib`.
- `no_startup_discovery` feature on the dial9 crate: feature-gates the startup-discovery path. When set, dial9 does not depend on `linkme`, the `SourceTag` impl uses the defaulted no-op, `Dial9Stream::new` does not iterate a registry and does not emit the empty-registry warn. Per-descriptor first-use validation continues to run.

## Validation failure policy

- First-use descriptor-local checks (InTrace-without-Dial9-source, InternString-on-non-string, Opaque-in-InTrace): `debug_assert!` panic in debug, rate-limited `tracing::error!` in release.
- Empty-registry warn: `tracing::warn!` in all builds. `no_startup_discovery` is the opt-out.
- Hand-written entries observed in the event path: rate-limited `tracing::warn!` once per distinct type id.
- Panic inside `Value::write`: caught per entry, rate-limited `tracing::warn!`, flush-thread state preserved.

## Public APIs at the boundary

The shape reviewers are agreeing to. Exact signatures may shift during implementation.

### In metrique

```rust
// metrique-writer-core / metrique re-exports
pub struct EntryDescriptor { /* ... */ }
pub struct FieldDescriptor { /* ... */ }
pub enum FieldShape { /* ... */ }
pub struct SourceDescriptor { /* ... */ }
pub struct SourceExtractor { /* ... */ }
pub trait Source<C> { type Snapshot; fn snapshot(&self) -> Self::Snapshot; }
pub trait SourceTag: Any + Send + Sync + 'static {
    fn register_descriptor(_desc: &'static EntryDescriptor) {}
}

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

impl metrique::SourceTag for Dial9 { /* register_descriptor overridden */ }

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

## Risks and mitigations

- **Metrique PR scope.** The descriptor work is non-trivial. Mitigation: the metrique design doc is explicit about what is in scope; metrique reviewers can veto pieces before dial9 starts consuming. Dial9 side can be scoped down if metrique ships a narrower initial API (e.g. no descriptor for enum entries).
- **Format-version compatibility.** Bumping to `VERSION = 2` requires updating every consumer. Mitigation: the bump is explicit, old readers reject cleanly rather than silently truncating; the viewer is in the same repo and can be updated lockstep.
- **Caller-thread cost regression.** `Dial9Context::capture()` does roughly the same work `TokioContextSink` did (a few TL reads plus `clock_monotonic_ns()`). We expect no measurable regression, but will benchmark during implementation.
- **`linkme` platform or linker-configuration problems.** Mitigation: `no_startup_discovery` opt-out feature; document the `-Wl,--whole-archive` gotcha.
