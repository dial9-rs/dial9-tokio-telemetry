# Metrique integration: implementation plan

**This document is deleted as part of PR sign-off. It captures what the implementation work looks like, in what order, in which files, and which design decisions each piece depends on.**

Status: as of this PR, nothing here is implemented. The dial9 work depends on a not-yet-opened metrique PR. This plan exists so reviewers can evaluate whether the scope and sequencing are sound.

## Sequencing

Work graph is three tracks with explicit dependencies. Tracks run in parallel where the graph permits.

### Track A: metrique descriptor + source system

Prerequisite for Tracks B and C. Tracked separately in the metrique repo; see `metrique/docs/entry-descriptors-impl-plan.md` for the full sub-track breakdown (M-A descriptor types, M-B erased-entry vtable hook, M-C macro + descriptor emission, M-D hand-written `DescribeEntry`).

Dial9 pins to a metrique release that has M-A, M-B, and M-C shipped. M-D is optional for dial9's initial work; dial9 users with hand-written entries benefit from it but dial9 does not require it to function.

### Track B: dial9 trace-format additions

Depends on nothing in Track A. Can start as soon as this design is approved.

- B1. Reserve a new top-level frame tag for schema annotations (e.g. `TAG_SCHEMA_ANNOTATIONS = 0x07`). No format version bump: old decoders halt on unknown tags, which matches our forward-compat posture. Ties to: dial9 keeper's "Trace format additions" section and review's Feasibility checks.
- B2. Add schema-level annotations: `FieldAnnotation { field_index, key, value }`, new annotation section in the schema frame, encoder/decoder support. Ties to: dial9 keeper's "Units" section.
- B3. Add typed dynamic map `FieldType` support. Single new variant `Map { key: FieldType, value: FieldType }`, decoded by walking key/value pairs using the schema-bound types. Recursion is forbidden: `Map` is not a valid `key` or `value`. Encoder does not write per-element type tags; schema is the single source of truth. Covers the full metrique Flex shape in one variant. Ties to: dial9 keeper's "Flex" and "Trace format additions → Typed dynamic maps" sections.
- B4. Update decoder / `FieldValue` / `FieldValueRef` for the new variants.
- B5. Update the dial9 viewer to render annotations and typed maps sensibly.
- B6. Regenerate the demo trace once the format is settled.

Parallelism within Track B: B1-B4 proceed together; B5-B6 come after B1-B4 stabilise.

### Track C: dial9 sink implementation

Depends on Track A (released metrique) and Track B (released trace-format).

- C1. `src/metrique/tags.rs`: `pub struct Dial9;`, `pub struct InTrace;`, `pub struct InternString;`. Plain `impl metrique::SourceTag for Dial9 {}` unconditionally. On supported targets, also `impl metrique::DiscoverableSourceTag for Dial9` pushing into `DIAL9_ENTRIES`, a `Mutex<Vec<&'static EntryDescriptor>>` populated by `linkme`. The `DiscoverableSourceTag` impl and `DIAL9_ENTRIES` are cfg-gated on target so that unsupported targets do not carry `linkme`. Ties to: dial9 keeper's "User-facing API" section; review's "Validation strategy" section.
- C2. `src/metrique/context.rs`: `Dial9Context` metrique field type with `#[metrics(source(Dial9))]` and `#[metrics(default_field_tag(skip(InTrace)))]`. `capture()` reads worker/task/monotonic. Closed form holds caller-thread + flush-thread snapshot. Ties to: dial9 keeper's "Components → `Dial9Context`" section.
- C3. `src/metrique/schema.rs`: `SchemaEntry` builder that takes an `EntryDescriptor` and produces the wire schema with annotations. Keyed cache on the `&'static EntryDescriptor` pointer. Ties to: dial9 keeper's "Components → Schema handling" section.
- C4. `src/metrique/writer.rs`: `Dial9EntryWriter` adapter walking `Entry::write` callbacks, cross-referencing the descriptor to filter by `InTrace`, routing `InternString` fields through the string pool, encoding per `FieldShape`. Ties to: dial9 keeper's architecture diagram "inside Dial9Stream" block.
- C5. `src/metrique/stream.rs`: `Dial9Stream` implements `EntryIoStream::next`. Descriptor-aware fast paths; per-descriptor first-use validation; `Dial9Stream::new` inspects the startup registry from C1 and emits the empty-registry warn. Ties to: dial9 keeper's "Validation → Startup-time" and "First-use" sections.
- C6. `src/metrique/builder.rs` + `src/metrique/mod.rs`: the three composition paths (global `attach_to_stream_with_dial9`, builder `metrique_sink`, manual `tee(emf, Dial9Stream::new(...))`). The manual path does not wrap the outer sink; caller-thread context capture is entirely in `Dial9Context`. Ties to: dial9 keeper's "User-facing API → Sink construction" section.
- C7. `Dial9Stream::builder(&handle).startup_discovery(bool)` runtime toggle. Default `true`. When `false`, the sink skips the empty-registry inspection at construction and never emits the warn for that sink. Per-descriptor first-use validation is unaffected. Ties to: dial9 keeper's "Validation → Startup-time" section; impl plan's "Startup-time discovery" section below.
- C8. Documentation: dial9 README section on the new API shape, `Dial9Context` usage, the `-Wl,--whole-archive` footnote for `cdylib` users.

Parallelism within Track C: C1-C5 can start concurrently once Track A is released. C6 depends on C1 and C5. C7 touches C1 and C5 (feature gating). C8 lags everything else.

### Testing (Track D)

Tests are authored alongside the code in each track.

- D1. Trace-format unit tests for schema annotations and typed dynamic maps. Include a "silent truncation" test: an old-style decoder reading a trace with a new top-level tag returns cleanly at end-of-stream, without error. In Track B.
- D2. Descriptor round-trip: a user-space struct with optionals, Flex, units, and tags; assert the sink registers exactly one schema with the expected annotations. In Track C.
- D3. Caller-thread context extraction: caller captures `Dial9Context`, passes through `BackgroundQueue`, flush-thread snapshot matches. In Track C.
- D4. Heterogeneous queue: a `BoxEntrySink<BoxEntry>` with multiple struct types, each gets one schema and one source extraction. In Track C.
- D5. Startup-time discovery: a binary with one `source(Dial9)` struct and a binary with none, assert the empty-registry warn fires in the second case only. Parallel run with `.startup_discovery(false)` verifies the warn is suppressed per-sink. Target-cfg gate is covered indirectly by the existing CI target matrix; WASM builds (if in CI) confirm dial9 compiles without `linkme`. In Track C.
- D6. Per-descriptor first-use validation: descriptors violating each check trigger `debug_assert!` in debug and rate-limited `tracing::error!` in release. In Track C.
- D7. Panic isolation: a `Value::write` that panics drops the offending event without poisoning the flush thread. In Track C.
- D8. End-to-end example in `examples/` producing a viewable trace with both runtime and metrique events. Last gate.

## Startup-time discovery: linkme, target gating, runtime toggle

`linkme` 0.3.x is the registration mechanism. Dial9 uses it via the `DiscoverableSourceTag` impl on `Dial9` to populate `DIAL9_ENTRIES` (a `Mutex<Vec<&'static EntryDescriptor>>`). Metrique uses `linkme` internally to drive the `register_descriptor` call per declared source. Neither appears in metrique's public API.

- **Target gating**. The `DiscoverableSourceTag` impl on `Dial9` and the `DIAL9_ENTRIES` static are both attached to a `#[cfg(any(target_os = "linux", target_os = "macos", target_os = "windows", target_os = "freebsd", target_os = "netbsd", target_os = "openbsd", target_os = "android"))]` gate. On other targets (WASM without appropriate features, exotic embedded, etc.) the impl and the static are compiled out; dial9 does not pull in `linkme` at all. The marker `SourceTag` impl exists unconditionally.
- **Dependency wiring**. `linkme` is declared as a target-conditional dependency in dial9's `Cargo.toml`: `[target.'cfg(any(target_os = "linux", ...))'.dependencies] linkme = "0.3"`. Nothing to toggle via cargo features.
- **Runtime per-sink toggle**. `Dial9Stream::builder(...).startup_discovery(false).build()` skips the registry inspection for that sink instance. Default is `true`. The toggle is a plain `bool` field on the builder.
- **Known gotchas**:
  - `cdylib` or `staticlib` users linking dial9 into a larger application may see empty registrations unless they pass `-Wl,--whole-archive` (or the platform equivalent) on the dial9 archive. Document in the dial9 README.
  - `cargo test` test binaries are separate from production binaries; registrations from tests do not leak. Registrations from the library under test are included because the test binary links the same `rlib`.

## Validation failure policy

- First-use descriptor-local checks (InTrace-without-Dial9-source, InternString-on-non-string, Opaque-in-InTrace): `debug_assert!` panic in debug, rate-limited `tracing::error!` in release.
- Empty-registry warn at sink construction: `tracing::warn!` in all builds. Disabled per-sink via `.startup_discovery(false)`; disabled at target level on unsupported platforms.
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
pub trait Extractable<C> { type Snapshot; fn snapshot(&self) -> Self::Snapshot; }
pub trait SourceTag: Any + Send + Sync + 'static {}
pub trait DiscoverableSourceTag: SourceTag {
    fn register_descriptor(registration: SourceRegistration<'static>);
}
#[non_exhaustive]
pub struct SourceRegistration<'a> { pub descriptor: &'a EntryDescriptor }

// Erased entry trait (method added to the existing dyn trait object behind BoxEntry)
fn descriptor(&self) -> Option<&'static EntryDescriptor>;

// Macro attributes
#[metrics(default_field_tag(T))]
#[metrics(default_field_tag(skip(T)))]
#[metrics(field_tag(T))]
#[metrics(field_tag(skip(T)))]
#[metrics(source(T))]
#[metrics(no_write)]
```

### In dial9

```rust
pub struct Dial9;
pub struct InTrace;
pub struct InternString;

impl metrique::SourceTag for Dial9 {}

// On supported targets only.
#[cfg(any(target_os = "linux", target_os = "macos", /* ... */))]
impl metrique::DiscoverableSourceTag for Dial9 {
    fn register_descriptor(reg: metrique::SourceRegistration<'static>) {
        DIAL9_ENTRIES.lock().unwrap().push(reg.descriptor);
    }
}

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
- **Format forward compatibility.** Additive extensions only; no format version bump. Old decoders halt at unknown tags, which means old viewers reading new traces will silently truncate at the first extension point. Mitigation: the viewer lives in-repo and updates with the format; users producing new traces update both together. Acceptable because the format is not widely distributed.
- **Caller-thread cost regression.** `Dial9Context::capture()` does a few thread-local reads plus `clock_monotonic_ns()`. Small fixed cost per entry; we will benchmark during implementation to confirm no measurable regression versus baseline metrique.
- **`linkme` platform or linker-configuration problems.** Mitigation: target-cfg gate on the `DiscoverableSourceTag` impl skips `linkme` entirely on unsupported targets. Per-sink `.startup_discovery(false)` silences the warn for users who hit legitimate false negatives. Document the `-Wl,--whole-archive` gotcha for `cdylib`/`staticlib` linking in the dial9 README.
