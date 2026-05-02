# Metrique Integration: Review-only Companion

**This document is deleted as part of PR sign-off. Anything that survives review lives in `metrique-integration.md`.**

The permanent design doc covers what we are building. This document covers why we picked it and what we rejected, so reviewers can evaluate the choice without reconstructing the reasoning from scratch.

## Requirements that drove the design

Hard constraints (a design that violated these would have been rejected outright):

1. **One sink composition works for heterogeneous entry types.** Users with multiple metrique struct types feeding the same pipeline cannot be forced to split sinks per type.
2. **Low net caller-thread overhead.** Encoding work does not happen on the caller. Net new cost on the caller thread is on the order of tens of nanoseconds.
3. **Tokio runtime context.** `worker_id`, `task_id`, and event timestamp are captured on the caller thread at close, not sampled later on a flush thread where the context is gone.
4. **Event timestamps.** The dial9 event frame timestamp reflects close time on the caller, not flush time.
5. **No metrique-core changes.** Metrique's core trait and type definitions stay untouched. It's nice to keep `metrique-macro` and `dial9-macro` untouched, but not required.
6. **Uniform handling of metrique field types.** `Duration`, `SystemTime`, `Timer`, `TimestampValue`, `Flex<T>`, value-string enums, and user custom types all work without per-type dial9 impls.

Strong downsides we worked to avoid (violations were possible but would have made the design meaningfully worse):

- **Non-local coupling for users.** Requiring users to annotate every metrique struct and also configure the sink was a significant downside. Annotating every individual struct would have been acceptable, if uncomfortable. Requiring both was the real problem we worked to eliminate.
- **No single user-facing entry point for the common case.** One call should build the sink. Advanced compositions remain possible but are not required.

Nice to have:

- Simple misuse-resistant APIs as the primary surface, with lower-level APIs allowed to expose more footguns to users who opt into manual composition.
- Compile-time misconfiguration checks, where cheap.
- Unit information in the trace via field-name convention.
- Clean Flex handling in v1 with bounded dynamic keys. Path to unbounded-key support.
- Future optimizations (schema caching, typed dynamic maps) without breaking the v1 user API.

## Tradeoffs in the chosen design

Items worth reviewer attention:

1. **Heterogeneous sinks vs. compile-time schema.** We picked runtime schema discovery, which costs on the order of 100 to 200 ns per event on the flush thread. The cost estimate is derived from hash-per-byte and trait-dispatch counts, not benched. Worth a microbenchmark before merge, especially for pathological cases (20+ field structs).
2. **Order-dependent shape fingerprint.** Depends on metrique's macro-generated `Entry::write` emitting fields in deterministic order per struct. True today but not a documented contract. If the order ever becomes nondeterministic per call, cache bloats. Functional but wasteful. We can add an integration test against current metrique that pins the assumption on our end, so we'd notice before users did.
3. **Bounded cache size default of 10,000.** Guess, not measured. Rough footprint: a schema entry is about 500 bytes (field defs, name strings, hash metadata), so 10,000 entries is about 5 MB of resident memory in the worst case. The cache size should become tunable via the builder (listed in the prospective improvements). Default should still be revisited against a real workload.
4. **Runtime warning for manual-composition misuse.** Relies on `tracing` being configured. Users running without `tracing` subscribed won't see the warning.

## Key design choices

### Runtime schema discovery rather than compile-time

Two blockers on the compile-time path, either sufficient on its own:

1. **Heterogeneous sinks**. The Global-sink path erases entries to `BoxEntry` before our stream sees them. A `Dial9Sink<E, S>` parameterized by a specific concrete `E` only works for homogeneous pipelines; users mixing metrique struct types through one pipeline would have to split their sinks.
2. **Open-ended `TraceField` maintenance**. Every `Value` impl in metrique (primitives, unit wrappers, aggregation types) would need a parallel `TraceField` impl. Every user custom type that today works with metrique by implementing `Value` alone would need a new `TraceField` impl to work with dial9. This is a cost users would pay on every custom type they add.

Runtime adaptation via `Entry::write` / `Value::write` handles anything metrique can emit, through metrique's stable abstraction, with no per-type maintenance. The cost is flush-thread work. The caller is unaffected.

### Context propagation via `EntryConfig`

`EntryConfig` is metrique's existing mechanism for per-entry format-specific metadata. Dial9 adds one type to this mechanism (`TokioContext`) and reads it from the config stream on the flush thread. No new metrique primitives required.

### `BoxEntry` erases concrete types

On the Global path, entries pass through `BoxEntrySink::append_any` which boxes into `BoxEntry`. The user's concrete entry type is erased by the time `Dial9Stream::next` runs. `TypeId::of::<E>()` at the stream level would always return `TypeId::of::<BoxEntry>()`, which is useless for schema-cache keying.

The schema cache therefore keys on shape fingerprint alone: a hash of observed `(name, field_type)` pairs in emission order. This also happens to handle Flex correctly. Different Flex keys produce different fingerprints and different cache entries.

### Order-dependent shape fingerprint

Metrique's macro-generated `Entry::write` emits fields in declaration order today. Two emissions of the same struct produce the same fingerprint. If metrique ever introduces nondeterministic emission order, the cache will bloat. Events still encode correctly, but the same struct occupies multiple cache slots. Order-independent fingerprinting is a drop-in replacement if we ever need it.

### Frame timestamp is captured on the caller thread

The dial9 event frame timestamp comes from `TokioContext::monotonic_ns`, captured at `TokioContextSink::append` time. This is close-time on the caller thread, in the same clock domain as tokio runtime events (`CLOCK_MONOTONIC`). Flush-thread timestamps would be lagged by however long the entry waited in the `BackgroundQueue` and would be unhelpful for correlation.

For users who compose `Dial9Stream` manually without `TokioContextSink`, `Dial9Stream` logs a rate-limited warning and falls back to the flush-thread clock. Functional but inferior. Users are expected to compose correctly.

## Alternatives considered

### Compile-time `TraceEvent` path (rejected)

Several shapes were explored, all variations on the theme of "dial9's compile-time `TraceEvent` machinery encodes metrique entries directly":

- A metrique-macro attribute, for instance `#[metrics(derive(TraceEvent), observe(Dial9))]` or the wrapper-macro variants (`#[dial9_metrics]`, `observe_as(TraceEvent => Dial9)`).
- Dial9 publishing `TraceField` implementations for metrique types (`Duration`, `SystemTime`, `Timer`, `TimestampValue`, `FlexEntry<T>`) plus a `TraceField` derive for value-string enums. Users extend for their own types.
- A compile-time pairing check inside metrique-macro that errors if a struct has `derive(TraceEvent)` without `observe(Dial9)`.

Two independent blockers, either of which would have been decisive:

**Blocker 1: the Global sink path erases `TraceEvent` through `BoxEntry`.** `BoxEntrySink::append_any` takes `impl Entry + Send + 'static` and calls `entry.boxed()` internally, so by the time `TokioContextSink` or `Dial9Stream` receives the entry, the user's concrete type has been erased to `BoxEntry`. `TraceEvent`-ness cannot be recovered from there. Preserving it would require either a parallel object-safe `DynTraceEvent` trait plus a dial9-owned box type (`TraceEvent` is not object-safe today: GAT `Ref<'a>`, generic `encode_fields<W: Write>`), or a metrique-side change so `ServiceMetrics` can attach a sink typed concretely over `TraceEvent + Entry`.

**Blocker 2: `TraceField` maintenance is open-ended and grows with metrique.** Every `Value` impl in metrique (primitives, unit wrappers, aggregation types, future additions) needs a parallel `TraceField` impl. More painfully, every user custom type that today works with metrique by implementing `Value` alone would need a new `TraceField` impl to work with dial9. That is a cost users pay forever, and dial9 cannot reduce it without coupling deeper into metrique's type system.

Runtime adaptation via `Entry::write` / `Value::write` dodges both. It operates on metrique's stable abstraction, which the boxed entry exposes just as well as the concrete one, and which every metrique-compatible type already implements.

Additional downsides that would have applied:

- Requires metrique-macro changes (passthrough, marker emission, pairing enforcement) to catch "I derived TraceEvent but didn't wire Dial9" misuse, and structs that forget the attribute are silently absent from dial9.
- Value-string enums cannot be covered by a single `TraceField` blanket without restricting what "being a value-string enum" means via an explicit metrique trait, which is another metrique change.

### Dial9 as a `Format`

Proposed shape: implement `metrique_writer_core::format::Format` or an `EntryIoStream` that acts as dial9's format inside a tee.

Partially accepted: `Dial9Stream` is an `EntryIoStream`. What was rejected:

- Running with no caller-thread context capture. `Format::format` and `EntryIoStream::next` are called on the flush thread. Thread-locals there reflect the flush thread, not the caller. We need `TokioContextSink` on the caller side to capture context in time.
- Running the full encoding pipeline with compile-time schema via `TraceEvent`. `Format::format` takes `&impl Entry`, not `&impl TraceEvent`. We cannot constrain entries to `TraceEvent` at the format boundary.
- Relying on flush-thread timestamps. Entry sat in a `BackgroundQueue` for an unbounded amount of time before the format sees it.

### Wrapping metrique's composition primitives

Proposed shape: dial9-owned `BackgroundQueue` and `tee` types that enforce composition correctness via type state, or require all dial9 users to go through dial9's versions.

Rejected because:

- Fragments the ecosystem. Users either use metrique's primitives or dial9's. Mixing is awkward. Composition with other metrique-writer features (`merge_globals`, `merge_global_dimensions`, sampling formats) becomes accidentally second-class.
- Enforcement coverage is marginal. The Global and Builder paths already prevent the misconfiguration that wrapping would catch. The only remaining misuse is in the Manual path, where users have explicitly opted into ad-hoc composition.

### Separate dial9-owned background thread for metrique events

Proposed shape: `TelemetryHandle::with_metrique_encoder()` at runtime-build time spawns a dial9-owned encoder thread. Caller sends boxed entries to it via an mpsc channel. Dial9 owns the thread. Metrique owns its own sink separately.

Rejected because:

- Two parallel background queues (one metrique-owned for EMF, one dial9-owned for dial9) doubles the thread count and complicates flush/shutdown semantics.
- Requires `E: Clone + Send + 'static` to send the entry across. A bound that most metrique entries satisfy but not all.
- The metrique-writer ecosystem is the natural home for the background work. Piggy-backing on metrique's `BackgroundQueue` means dial9 benefits from queue metrics, flush coordination, shutdown timeouts, and metrics-rs integration for free.

### Wire format changes (deferred)

Proposed shape: new `FieldType` variants for typed dynamic maps (e.g., `StringMapOfF64`) that would encode Flex fields as a single typed-map field per parent entry rather than one distinct schema per dynamic key.

Deferred rather than rejected. This is in the prospective improvements list of the design doc.

Reasoning for deferral:

- No v1 user is asking for it. Bounded cache with eviction handles the common cases.
- Wire format changes are additive but not free: every trace consumer that inspects field types has to be updated. v1's job is to get metrique events into traces. The encoding choice for highly dynamic Flex usage is a separate conversation that needs real-world v1 usage data.

### Programmatic stats handle in v1

Proposed shape: `Dial9StatsHandle` returned from the builder, holding an `Arc<StatsInner>` shared with `Dial9Stream`, with a `snapshot()` method.

Rejected for v1 because:

- Doesn't compose cleanly with the Global path. `AttachHandle` is opaque and can't carry additional handles without changing metrique.
- Shape of the desired stats API is unclear without real user pressure. Histogram vs counters, per-schema breakdown vs aggregate, push vs pull.
- `tracing::debug!` plus rate-limited `tracing::warn!` on eviction is enough to diagnose problems in v1. Integrating into a proper metrics pipeline can happen once [metrique#205](https://github.com/awslabs/metrique/issues/205) lands a better reporting hook.

## Feasibility checks completed

The design rests on specific metrique and dial9 API shapes, confirmed before finalizing.

- `AttachGlobalEntrySink::attach` takes `(impl EntrySink<BoxEntry> + Send + Sync + 'static, impl Any + Send + Sync)`. `BackgroundQueue::new(stream)` produces that tuple. The Global path hands the same tuple to `attach`.
- `tee` and `BackgroundQueue` are public. `tee(stream1, stream2)` implements `EntryIoStream`; `BackgroundQueue::new(stream)` wraps any `EntryIoStream`, so `BackgroundQueue::new(tee(emf, dial9))` is the composition the Global and Builder paths use.
- `EntryConfig` is `Any + Debug + 'static`. Custom config types (our `TokioContext`) slot in without metrique changes.
- `EntryIoStream::next(&mut self, entry: &impl Entry)` is where our runtime adapter hooks. `Entry::write(writer)` drives the adapter.
- `ValueWriter::metric(..., unit: Unit, ...)` exposes the unit for numeric values. `Unit::name()` returns `&'static str` in CloudWatch's vocabulary.
- `dial9_tokio_telemetry::telemetry::clock_monotonic_ns()` is `pub` and callable from any thread. `TokioContextSink` depends on it.
- `FlexEntry<T>::write` emits exactly one `value(dynamic_key, &value)` call. Different dynamic keys produce different shape fingerprints naturally.
- `ThreadLocalEncoder` on the flush thread is how dial9 events are written. It wraps `dial9_trace_format::encoder::Encoder`, which exposes `pub fn write_event(schema, values)` for the dynamic-schema path.
- `BoxEntry::inner()` returns `&dyn Any`, but the inner is the trait object, not the user's struct. `Any::type_id()` is useless for distinguishing user entry types through `BoxEntry`. Schema cache therefore keys on shape fingerprint, not `TypeId`.
