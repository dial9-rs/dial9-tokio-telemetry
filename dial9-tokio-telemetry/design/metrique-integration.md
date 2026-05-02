# Metrique Integration

Dial9 integrates [metrique](https://github.com/awslabs/metrique) application-level metric entries into dial9 traces, so a single trace file can carry both tokio runtime telemetry and per-request application metrics. Users configure dial9 once at sink construction. Every entry that flows through the configured sink is recorded into the dial9 trace in addition to its normal destination (EMF or JSON to disk, etc).

## User-facing API

Three ways to use the integration. The **Global** path covers the expected common case, **Builder** is for standalone sinks, **Manual** is for ad-hoc composition.

### Global sink attach

```rust
use dial9_tokio_telemetry::AttachDial9Ext;
use metrique::ServiceMetrics;
use metrique_writer::FormatExt;
use metrique_writer_format_emf::Emf;
use tracing_appender::rolling::{RollingFileAppender, Rotation};

let emf_stream = Emf::all_validations("MyApp".into(), vec![vec![]])
    .output_to_makewriter(RollingFileAppender::new(
        Rotation::HOURLY, log_dir, "metrics.log",
    ));

let _handle = ServiceMetrics::attach_to_stream_with_dial9(
    emf_stream,
    &telemetry_handle,
);
```

`attach_to_stream_with_dial9` composes `tee(emf_stream, Dial9Stream)` inside a `BackgroundQueue`, wraps the outer sink with `TokioContextSink`, and attaches the result to `ServiceMetrics`. Internals are not exposed to the user.

### Builder (standalone sink)

```rust
use dial9_tokio_telemetry as dial9;
use metrique_writer::sink::BackgroundQueueBuilder;

let emf_stream = Emf::all_validations("MyApp".into(), vec![vec![]])
    .output_to_makewriter(RollingFileAppender::new(
        Rotation::HOURLY, log_dir, "metrics.log",
    ));

let (sink, _join) = dial9::metrique_sink(emf_stream, &telemetry_handle).build();

RequestMetrics { /* ... */ }.append_on_drop(sink.clone());
```

Same composition as the Global path, but returned as a concrete sink. Builder variants cover alternate inner sinks and customized `BackgroundQueue` configuration:

```rust
let (sink, _join) = dial9::metrique_sink(emf_stream, &telemetry_handle)
    .flush_immediately()
    .build();

let (sink, _join) = dial9::metrique_sink(emf_stream, &telemetry_handle)
    .with_background_queue_builder(BackgroundQueueBuilder::new().capacity(2048))
    .build();

let (sink, _join) = dial9::metrique_sink(emf_stream, &telemetry_handle)
    .with_sink(|stream: impl EntryIoStream| FlushImmediately::new(stream))
    .build();
```

The `.with_sink` closure takes the tee'd `EntryIoStream` (EMF + `Dial9Stream`) and returns any `EntrySink` the user wants wrapping it.

### Manual composition

```rust
use dial9_tokio_telemetry::{Dial9Stream, TokioContextSink};
use metrique_writer::sink::BackgroundQueue;
use metrique_writer::stream::tee;

let stream = tee(emf_stream, Dial9Stream::new(&telemetry_handle));
let (bg, _join) = BackgroundQueue::new(stream);
let sink = TokioContextSink::new(bg);

RequestMetrics { /* ... */ }.append_on_drop(sink.clone());
```

The two primitives (`Dial9Stream`, `TokioContextSink`) are public. If `TokioContextSink` is not in the pipeline, `Dial9Stream::next` falls back to the flush-thread `clock_monotonic_ns()` for the event timestamp, records `WorkerId::UNKNOWN` with no task ID, and emits a rate-limited `tracing::warn!` about the missing context. Events still flow.

### Metrique struct code is unchanged

```rust
#[metrics(rename_all = "PascalCase")]
struct RequestMetrics {
    operation: String,
    #[metrics(unit = Millisecond)]
    latency: Timer,
    success: bool,
    #[metrics(flatten)]
    counters: Flex<u64>,
}
```

No attributes, no derives, no per-struct wiring. The design deliberately avoids coupling at the `#[metrics]` layer.

## Why runtime schema discovery

`Dial9Stream` learns each entry's schema at encode time by walking `Entry::write` through an adapter, rather than relying on dial9's compile-time `#[derive(TraceEvent)]` machinery. Two independent blockers, either sufficient on its own:

1. The Global sink path erases entries to `BoxEntry` before any dial9-owned wrapper sees them, so `TraceEvent`-ness cannot be recovered through `ServiceMetrics`.
2. A compile-time path would require a `TraceField` impl for every metrique primitive, every metrique-writer unit wrapper, and every user custom type. That is unbounded maintenance.

Runtime adaptation works through `Value`, which is metrique's stable abstraction for field emission. Anything metrique can write to EMF today, `Dial9Stream` can encode. The cost is flush-thread work, not caller-thread work.

## Components

### `TokioContextSink<S>`

Sink wrapper. On `append(entry)`:

1. Reads `clock_monotonic_ns()` and the tokio runtime thread-locals (`current_worker_id()`, `tokio::task::try_id()`) into a `TokioContext { monotonic_ns, worker_id, task_id }`.
2. Wraps the entry in `WithContext<E> { inner: E, ctx: TokioContext }`. `TokioContext` is `Copy`, and the wrapper is a stack struct with no allocation.
3. Forwards `WithContext<E>` to the inner sink.

`WithContext<E>` implements `Entry`. Its `write()` emits `writer.config(&self.ctx)` first and then delegates to `self.inner.write(writer)`.

Net new caller-thread cost is on the order of tens of nanoseconds: a few thread-local reads and a clock syscall. No allocations beyond what the inner sink already does. `TelemetryHandle` is cloned once at `Dial9Stream::new` and held for the stream's lifetime; the clone is an `Arc` bump, not per-event.

### `TokioContext`

```rust
pub struct TokioContext {
    pub monotonic_ns: u64,
    pub worker_id: WorkerId,
    pub task_id: Option<TaskId>,
}
impl EntryConfig for TokioContext {}
```

Plain `Copy` struct that rides through the sink pipeline as an `EntryConfig`. Formats that recognize the type downcast to it. Formats that do not recognize it ignore it, per the standard `EntryConfig` contract.

### `Dial9Stream`

`EntryIoStream` implementor, constructed with a `TelemetryHandle`. Runs on whatever thread metrique's pipeline calls `next` on. For the Global and Builder paths above, this is the `BackgroundQueue` flush thread. On `next(entry)`:

1. If the handle is inert (no dial9 runtime attached, or the runtime is disabled): return `Ok(())` immediately. No field walk, no cache lookup, no work.
2. Walks `entry.write(&mut adapter)` where `adapter` is an internal `EntryWriter` implementation.
3. The adapter's `config()` method picks `TokioContext` out of the config stream.
4. The adapter's `value(name, value)` method resolves each metrique `Value` through an internal `ValueWriter` that captures `(FieldType, encoded_bytes, Unit)`.
5. Field names receive a unit suffix when the unit is not `Unit::None`: `latency_Milliseconds`, `payload_size_Bytes`, and so on. The suffix uses `Unit::name()` verbatim. Some unit names contain `/` (e.g. `Bytes/Second`) or arbitrary user text (`Unit::Custom`), producing suffixes like `throughput_Bytes/Second` that downstream consumers must handle or expect normalization on (see Prospective improvements).
6. Computes an order-dependent shape fingerprint over `(name, field_type)` pairs. Looks up or registers a `dial9_trace_format::encoder::Schema` in a bounded cache.
7. Calls `TelemetryHandle::with_encoder` and writes the event via the schema-driven `write_event` path. The frame timestamp is `TokioContext::monotonic_ns` from the caller thread. The `entry.write` walk is wrapped in `catch_unwind` so a panic inside a user's `Value::write` impl drops the offending event (rate-limited log) without poisoning the flush thread's TL buffer mutex. The boundary uses `AssertUnwindSafe` since `Entry` has no unwind-safety bound; a panicked event is dropped, not retried.

Steady-state flush-thread cost per event is on the order of a few hundred nanoseconds for schema handling and encode. This is off the caller's critical path when the `BackgroundQueue` is in play (Global and Builder paths).

### Bounded schema cache

Fixed capacity, default 10,000 entries, LRU eviction. LRU eviction rate-limits to a `tracing::warn!` so operators can tell when the cache is thrashing.

What the cache counts as a distinct entry:

- **Static struct shape**: one entry per distinct `Entry::write` call pattern. A struct with `K` `Option<T>` fields produces up to `2^K` distinct cache entries as those fields come and go across emissions (each `Option<T>::write` is a no-op when `None`). For typical metrique structs (1-3 optional fields) that is 2-8 entries per struct type, well within the cap.
- **Flex keys**: each distinct `Flex::new(key)` string is a distinct cache entry. With `M` Flex fields in a struct and `N` distinct keys per field, cardinality is `N^M`.

Thrash cost when the working set exceeds the cap is real, not "graceful". Each cache miss registers a fresh schema with the encoder, which writes a schema frame (roughly 100-150 bytes for a 5-field struct, dominated by the field name strings) and re-interns any previously-evicted field names. On a workload that keeps missing, every emission pays this overhead on top of the event payload itself; the trace wire volume can double or more.

In practice, Flex is used sparingly and with low per-field cardinality, so typical usage stays well under the cap. The prospective Flex field promotion (below) replaces unbounded-key Flex with a single typed-map schema and eliminates the thrash path entirely.

### Observability

- Periodic `tracing::debug!` from `Dial9Stream` reporting schema cache size and cumulative counters (registrations, evictions, events emitted). Off at `info` by default. Users enable by raising the dial9 module to `debug`.
- Rate-limited `tracing::warn!` on LRU eviction.

No programmatic stats handle in v1.

## Pipeline architecture

The only work that runs on the caller thread is capturing `TokioContext` and wrapping the entry. Encoding runs on the `BackgroundQueue` flush thread alongside EMF formatting.

```
  CALLER THREAD                               │   FLUSH THREAD
  (tokio worker, typically)                   │   (BackgroundQueue worker)
                                              │
  RequestMetrics.append_on_drop(sink)         │
        │                                     │
        ▼                                     │
  TokioContextSink::append                    │
    ├── capture monotonic_ns + worker + task  │
    └── wrap entry with TokioContext config   │
        │                                     │
        ▼                                     │
  BackgroundQueue mpsc push  ──────────────── │ ──►  BackgroundQueue::next(entry)
                                              │       │
                                              │       ▼
                                              │     tee: EntryIoStream::next
                                              │       ├─► EMF format (uses its
                                              │       │   own config, ignores
                                              │       │   TokioContext)
                                              │       │
                                              │       └─► Dial9Stream::next
                                              │             ├─ adapter reads
                                              │             │  TokioContext
                                              │             ├─ walk fields
                                              │             ├─ schema lookup
                                              │             └─ encode into
                                              │                TL buffer
                                              │
                                              │   (dial9's normal drain
                                              │    machinery takes over from
                                              │    the TL buffer)
```

`TokioContext` is captured where it is available (caller thread) and propagated through `EntryConfig` so that dial9 encoding on the flush thread uses the correct worker, task, and timestamp.

## Resilience

- **Thread not owned by a dial9 runtime**: `TokioContextSink` captures whatever the thread-locals return (`WorkerId::UNKNOWN`, `task_id = None`). The event flows with those values. If the telemetry handle is inert (no runtime attached), `Dial9Stream::next` short-circuits and does no work; the entry still reaches EMF through the tee.
- **Dial9 runtime disabled after construction**: `Dial9Stream::next` short-circuits on the inert handle. Entries still flow to EMF via the tee. Matches dial9's existing disabled-handle semantics.
- **Flush thread is not a tokio worker**: expected. The flush thread holds its own dial9 thread-local buffer once it emits. Dial9's existing drain machinery handles flush-thread buffers like any other.

## Prospective improvements

These are possible follow-ons. They are directions the initial API shape was chosen to accommodate.

### Flex field promotion

Today, each distinct Flex key produces its own cached schema. The bounded cache plus LRU eviction handles bounded-cardinality Flex workloads gracefully, but unbounded keys churn the cache and re-emit schema bytes per emission.

Promotion would detect the "many schemas differing only at one field position, one Flex value type" pattern and collapse those schemas into a single schema with a typed dynamic-map field. Dependencies:

- A new `dial9-trace-format` wire type (typed-map variants, or a generic `TypedMap(FieldType)`).
- Promotion heuristics inside `Dial9Stream`. Heuristics are avoidable if metrique signals that a field is a Flex field (see "Metrique-side changes" below).

### Metrique-side changes that would improve the integration

- **Flex field signal on `EntryWriter`**. A method like `EntryWriter::flex_value(name, value)` lets dial9 identify Flex fields without inferring from schema shape. Removes the need for promotion heuristics and lets the adapter emit the right wire type on the first emission.
- **Stable `&'static str` field name contract**. If metrique publishes that macro-generated field names are `&'static`, `Dial9Stream` can skip content hashing for those names on the shape-fingerprint path.
- **Field emission order guarantee**. A documented contract that `Entry::write` emits fields in declaration order per struct would let us drop "order-dependent" as a caveat on the fingerprint.
- **Better stats reporting hook**. Once metrique exposes a more flexible metric-reporting hook for `BackgroundQueueBuilder` ([metrique#205](https://github.com/awslabs/metrique/issues/205)), `Dial9Stream` could surface its cache size, registration, and eviction counters through the same mechanism.

### Dial9-side improvements

- **Schema-cache size tunability in the public builder**: surface the cache capacity on `metrique_sink(...)` so operators can size it for their workload. Trivial addition; no API rework.
- **Integration with format-layer sampling**: metrique's `FixedFractionSample` and `CongressSample` wrap a `Format`, not an `EntryIoStream`. If `Dial9Stream` were reshaped as a `Format` or paired with one, high-frequency metric events could be sampled away from the dial9 path independently of EMF.
- **Structured units on the wire**: `Unit::name()` verbatim in the field name is fine for v1, but future work could either normalize the suffix or extend the wire format to carry units as structured metadata. The v1 adapter can switch to whichever lands without an API change.