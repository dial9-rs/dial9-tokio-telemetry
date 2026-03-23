# Thread-Local Encoding with Central Transcoding

## Overview

Move event encoding from the flush thread into per-worker
thread-local buffers. Each worker thread encodes `RawEvent`s
into wire-format bytes locally, then flushes pre-encoded byte
buffers to the `CentralCollector`. The flush thread transcodes
(reconciles string pools, rebases timestamps) and writes the
final bytes to disk.

This enables dynamic event types: any code with access to the
thread-local `Encoder` can define and emit new event schemas
at runtime without modifying the central `RawEvent` enum or
the flush-thread encoding logic. It also distributes encoding
work across all worker threads, where it can overlap with I/O
waits (park/unpark).

## Current CX / Concepts

### Data flow today

1. Worker threads create `RawEvent` enums (~48 bytes each) in
   hot-path callbacks (`on_before_task_poll`,
   `on_after_task_poll`, `on_thread_park`, `on_thread_unpark`,
   etc.) via `SharedState::record_event`.
2. Events are pushed into a `ThreadLocalBuffer`
   (`Vec<RawEvent>`, capacity 1024 in `buffer.rs`).
3. When the buffer reaches 1024 events, the entire `Vec` is
   flushed to `CentralCollector` (a crossbeam `ArrayQueue`
   with capacity 1024 batches in `collector.rs`).
4. A dedicated `dial9-flush` thread wakes every 5 ms, drains
   the `CentralCollector`, and for each `RawEvent` calls
   `EventWriter::write_raw_event` →
   `RotatingWriter::write_resolved` →
   `Encoder<BufWriter<File>>::write`.
5. `write_resolved` converts each `RawEvent` to wire format:
   string interning, varint encoding, and timestamp delta
   encoding.

### Key types

| Type | Location | Role |
|------|----------|------|
| `RawEvent` | `events.rs` | Enum (~48 B) with rich data (`&'static Location`, `usize` queue depths) |
| `ThreadLocalBuffer` | `buffer.rs` | `Vec<RawEvent>` + `Option<Arc<CentralCollector>>` |
| `CentralCollector` | `collector.rs` | `ArrayQueue<Vec<RawEvent>>`, capacity 1024 batches |
| `Encoder<W>` | `dial9-trace-format/encoder.rs` | Schema registration, string interning, timestamp delta encoding |
| `RotatingWriter` | `writer.rs` | Owns `Encoder<BufWriter<File>>`, handles file rotation/eviction |
| `EventWriter` | `event_writer.rs` | Intermediate layer owning `Box<dyn TraceWriter>` + CPU profiler |

### Motivation

Today, every event type must be a variant of the `RawEvent`
enum. Adding a new event type requires modifying `RawEvent`,
adding encoding logic to `RotatingWriter::write_resolved`,
and updating the decoder. This is rigid — it prevents
libraries, plugins, or runtime-generated instrumentation from
emitting custom event types into the trace.

Thread-local encoding solves this: each worker thread owns an
`Encoder<Vec<u8>>` that can register arbitrary schemas and
encode events directly. New event types only need a
`#[derive(TraceEvent)]` struct — no changes to the central
enum or flush path. The flush thread transcodes opaque byte
batches without knowing the event types they contain.

A secondary benefit is distributing encoding CPU across
worker threads, reducing flush-thread load under high event
rates.

### Complications

- **String interning**: `Encoder` maintains a
  `HashMap<String, u32>` string pool. Per-thread encoders
  produce thread-local pool IDs that must be remapped when
  transcoding into the central file.
- **Timestamp delta encoding**: `Encoder` tracks
  `timestamp_base_ns` and encodes deltas as u24. Per-thread
  encoders have independent bases. The flush thread must
  re-encode timestamps relative to the file's base.
- **Schema registration**: Each per-thread encoder must emit
  schema frames. The flush thread must deduplicate schemas
  and remap type IDs.

## Proposed CX / CX Specification

No user-facing API changes. The optimization is entirely
internal. Existing `TracedRuntimeBuilder`, `TelemetryGuard`,
and `TelemetryHandle` APIs remain unchanged. The wire format
on disk is identical — traces produced before and after this
change are interchangeable.

### Observable behavior changes

- **Lower flush-thread CPU usage**: encoding work shifts to
  worker threads.
- **Higher per-event cost on worker threads**: each
  `record_event` call now encodes to bytes (~50–100 ns)
  instead of just pushing an enum (~5–10 ns). This happens
  during park/unpark and poll callbacks, which are already
  in the noise relative to actual task work.
- **Smaller cross-thread transfers**: encoded bytes are
  ~8–15 bytes per event vs. ~48 bytes per `RawEvent`,
  reducing cache-line traffic through the `ArrayQueue`.
- **Reduced batch-drop rate under load**: the flush thread
  does less work per batch, so it drains faster.
- **Dynamic event types**: any `#[derive(TraceEvent)]`
  struct can be encoded on a worker thread without changes
  to the central pipeline.
- **Bounded memory consumption**: Since the exact buffer sizes (and the quantity of active bufffers is known) we can effectively put a hard cap on the amount of memory consumed by dial9.
- **Prepares for decoupling from Tokio**: In the fullness of time, dial9 will operate as a centralized flight recording system where Tokio is just one participant

## Technical Design

### Dual-path architecture

During incremental migration, each `ThreadLocalBuffer`
holds both paths simultaneously:

- A `Vec<RawEvent>` for event types not yet migrated to
  thread-local encoding.
- An `Encoder<Vec<u8>>` for event types that have been
  migrated.

Each call to `record_event` routes the event to one path
or the other based on the event variant. For example,
`PollStart` might encode through the `Encoder` while
`QueueSample` still goes into the `Vec<RawEvent>`. As
event types are migrated one by one, the `Vec<RawEvent>`
shrinks and the `Encoder` handles more. Eventually, only
`CpuSample` events (produced on the flush thread, not
worker threads) remain as `RawEvent`s.

This enables:

1. **Incremental migration**: existing event types move
   to thread-local encoding one at a time, with no
   flag-day cutover.
2. **Benchmarking**: the `RawEvent` path serves as a
   baseline for measuring the performance impact of
   thread-local encoding.
3. **CPU profiler events**: `CpuSample` events are
   produced on the flush thread and written directly
   through the central encoder — they never touch the
   thread-local path.

On flush, the buffer produces a `Batch` containing both
the raw events and the encoded bytes from that session:

```rust
// collector.rs
pub(crate) struct Batch {
    /// Unencoded events (legacy path / not-yet-migrated).
    pub raw_events: Vec<RawEvent>,
    /// Pre-encoded byte stream from the thread-local
    /// Encoder (migrated events). Empty if no events
    /// were routed to the encoder this session.
    pub encoded_bytes: Vec<u8>,
}
```

The flush thread processes both halves of each batch:

```rust
// flush_once (sketch)
while let Some(batch) = shared.collector.next() {
    for raw in &batch.raw_events {
        event_writer.write_raw_event(raw)?;
    }
    if !batch.encoded_bytes.is_empty() {
        transcoder::transcode(
            &batch.encoded_bytes,
            &mut event_writer.encoder(),
        )?;
    }
}
```

### Phase 1: Thread-local `Encoder<Vec<u8>>`

Add an `Encoder<Vec<u8>>` alongside the existing
`Vec<RawEvent>` in `ThreadLocalBuffer`. Both paths
coexist in the same buffer:

```rust
// buffer.rs (new)
pub(crate) struct ThreadLocalBuffer {
    /// Legacy path: events not yet migrated to
    /// thread-local encoding.
    events: Vec<RawEvent>,
    /// New path: thread-local encoder for migrated
    /// event types.
    encoder: Encoder<Vec<u8>>,
    event_count: usize,
    collector: Option<Arc<CentralCollector>>,
    /// Cache: Location → formatted String, avoids
    /// reformatting the same spawn location on every
    /// PollStart. Uses the Location value (which impls
    /// Hash + Eq) as key, matching the existing
    /// RotatingWriter::formatted_locations pattern.
    /// Caches the formatted String (not InternedString)
    /// so only the intern_string call is repeated after
    /// encoder reset, avoiding Location::to_string().
    location_cache: HashMap<
        std::panic::Location<'static>,
        String,
    >,
}
```

`record_event` routes each event to the appropriate path:

```rust
fn record_event(&mut self, event: RawEvent) {
    if Self::should_encode(&event) {
        self.encode_event(&event);
    } else {
        self.events.push(event);
    }
    self.event_count += 1;
}

/// Returns true for event types that have been migrated
/// to thread-local encoding. Initially returns false for
/// all types; as each type is migrated, its match arm
/// flips to true.
fn should_encode(event: &RawEvent) -> bool {
    match event {
        // Migrated events:
        // RawEvent::PollStart { .. } => true,
        // Not yet migrated:
        _ => false,
    }
}
```

The `encode_event` method mirrors the current
`RotatingWriter::write_resolved` logic but writes to the
in-memory `Vec<u8>` encoder. String interning uses the
thread-local encoder's pool — each thread assigns its own
pool IDs independently.

On flush (when `event_count >= BUFFER_CAPACITY`), the
buffer produces a `Batch` containing both halves and
resets both paths:

```rust
fn flush(&mut self) -> Batch {
    let raw_events = std::mem::replace(
        &mut self.events,
        Vec::with_capacity(BUFFER_CAPACITY),
    );
    let encoded_bytes = self.encoder.reset_to(
        Vec::with_capacity(ESTIMATED_BATCH_SIZE),
    );
    self.event_count = 0;
    // location_cache is preserved — it caches formatted
    // Strings, not InternedString IDs, so it survives
    // encoder resets.
    Batch { raw_events, encoded_bytes }
}
```

### Phase 2: `Batch` in `CentralCollector`

`CentralCollector` changes from
`ArrayQueue<Vec<RawEvent>>` to `ArrayQueue<Batch>`:

```rust
pub fn accept_flush(&self, batch: Batch) {
    if let Some(_evicted) =
        self.queue.force_push(batch)
    {
        self.dropped_batches
            .fetch_add(1, Ordering::Relaxed);
    }
}
```

### Phase 3: `Encoder::reset_to`

Add a `reset_to` method to `Encoder<W>` in
`dial9-trace-format` that resets internal state and swaps
the writer, returning the old writer's output. This avoids
reallocating the `HashMap` and `SchemaRegistry` on every
flush.

```rust
// dial9-trace-format/encoder.rs
impl<W: Write> Encoder<W> {
    /// Reset the encoder into a new writer, preserving
    /// internal allocations (string pool HashMap, schema
    /// registry, schema_ids map). Returns the previous
    /// writer.
    ///
    /// The string pool, schema registry, and schema_ids
    /// are cleared but their allocations are reused.
    /// A new file header is written to `new_writer`.
    pub fn reset_to(
        &mut self,
        mut new_writer: W,
    ) -> W {
        codec::encode_header(&mut new_writer)
            .expect("header write failed");
        self.string_pool.clear();
        self.next_pool_id = 0;
        // Clear the registry's HashMap in place to
        // preserve its allocation.
        self.registry.schemas.clear();
        self.registry.next_id = 0;
        self.schema_ids.clear();
        let old_state = std::mem::replace(
            &mut self.state,
            EncodeState::new(new_writer),
        );
        old_state.writer.into_inner()
    }
}
```

For `Encoder<Vec<u8>>`, a convenience method returns the
encoded bytes directly:

```rust
impl Encoder<Vec<u8>> {
    /// Reset the encoder, returning the encoded bytes
    /// and starting a fresh encoding session.
    pub fn reset(&mut self) -> Vec<u8> {
        self.reset_to(Vec::new())
    }
}
```

### Phase 4: First-class transcoding in `dial9-trace-format`

Transcoding — decoding one encoded stream and re-encoding
it through a different encoder — is a first-class operation
in `dial9-trace-format`, not an ad-hoc implementation in
the telemetry crate.

Transcoding is a **free function**, not a struct. The key
insight is that there is no useful state to carry across
calls:

- `values_buf: Vec<FieldValueRef<'_>>` borrows from the
  `source` bytes, so its lifetime is tied to each call.
  It cannot live on a struct.
- A `Vec<FieldValue>` (owned) would work on a struct but
  forces copying every string and byte slice from the
  source — defeating the purpose of zero-copy decoding.
- Caching a 10-element vec across calls saves nothing
  meaningful.

```rust
// dial9-trace-format/src/transcoder.rs

/// Transcode all events from `source` bytes into
/// `target` encoder. Handles:
/// - Schema deduplication and type ID remapping
/// - String pool ID remapping
/// - Timestamp rebasing (decode to absolute, re-encode
///   as delta from target encoder's base)
///
/// Zero-copy: field values are re-encoded directly from
/// FieldValueRef borrows into the source buffer. Only
/// pooled string IDs are remapped (hash lookup, no copy).
pub fn transcode<W: Write>(
    source: &[u8],
    target: &mut Encoder<W>,
) -> Result<(), TranscodeError> { ... }

#[derive(Debug)]
pub enum TranscodeError {
    InvalidHeader,
    Decode(DecodeError),
    Io(io::Error),
}
```

The function manually iterates decoder frames rather than
using `try_for_each_event`. This is necessary because the
transcoder needs simultaneous access to:

1. The decoder (to advance through frames and access the
   string pool)
2. A `HashMap<WireTypeId, Schema>` built incrementally as
   schema frames are encountered
3. The target encoder (to write events)

With `try_for_each_event`, the decoder is mutably borrowed
for the entire iteration, preventing access to its registry
from inside the callback. The manual loop avoids this by
interleaving decoder state access with event processing.

The implementation:

1. Creates a `Decoder` from `source`.
2. Iterates frames by tag byte:
   - **Schema frames**: processed via `next_frame_ref()`
     (which updates decoder state), then a `Schema` handle
     is built from the `SchemaEntry` and stored in a local
     `HashMap<WireTypeId, Schema>`.
   - **String pool frames**: processed via
     `next_frame_ref()` (decoder builds its pool).
   - **Event frames**: decoded inline (same logic as
     `for_each_event`), pooled string IDs remapped through
     `target.intern_string()`, then re-encoded via
     `target.write_event_ref(schema, timestamp, &values)`.
   - **Timestamp resets**: update decoder's timestamp base.
3. `values_buf: Vec<FieldValueRef<'_>>` is local to the
   function, reused across events via `clear()`.

#### New encoder APIs for transcoding

`Encoder::write_event_ref` accepts `&[FieldValueRef<'_>]`
directly, avoiding conversion to owned `FieldValue`:

```rust
impl<W: Write> Encoder<W> {
    pub fn write_event_ref(
        &mut self,
        schema: &Schema,
        timestamp_ns: u64,
        values: &[FieldValueRef<'_>],
    ) -> io::Result<()> { ... }
}
```

Unlike `write_event` (which extracts the timestamp from
`values[0]`), `write_event_ref` takes the timestamp as a
separate argument since the decoder already provides it.

`EventEncoder::write_field_value_ref` encodes each
`FieldValueRef` variant directly. For `StackFrames` and
`StringMap`, it writes the raw backing bytes via new
`raw_data()` accessors on `StackFramesRef` and
`StringMapRef`, achieving true zero-copy for these
variable-length types.

#### `try_for_each_event`

Added alongside `for_each_event` on `Decoder` for other
use cases that need fallible callbacks:

```rust
pub fn try_for_each_event<E>(
    &mut self,
    f: impl for<'f> FnMut(RawEvent<'a, 'f>)
        -> Result<(), E>,
) -> Result<(), TryForEachError<E>>
```

Not used by the transcoder itself (which needs the manual
loop), but useful for consumers that don't need schema
access during iteration.

### Event type ID overflow (>255 types)

The current wire format uses `WireTypeId(u16)` for event
type IDs in both schema and event frames. This supports up
to 65,535 unique types per file, which is sufficient for
the foreseeable future.

However, the event frame layout packs the type ID (2 bytes)
and timestamp delta (3 bytes) into a fixed 6-byte header
(1 tag + 2 type_id + 3 delta). With dynamic event types,
the number of schemas per encoding session could grow. If
we ever need to support more than 65,535 types, the format
would need an extension.

The proposed approach for future-proofing: reserve
`WireTypeId(0xFFFF)` as an overflow sentinel. When the
type ID is `0xFFFF`, the actual type ID follows as a
LEB128-encoded u32 after the timestamp delta:

```
[TAG_EVENT] [0xFF 0xFF] [u24 delta] [LEB128 type_id]
```

This is backward-compatible: existing decoders that never
see type ID 0xFFFF continue to work. New decoders check
for the sentinel and read the extended ID.

This overflow mechanism is not implemented in the initial
version — it is documented here as the planned extension
point. The current u16 range (65,535 types) is sufficient
for thread-local encoding where each per-thread encoder
session typically uses <20 types.

### String pool reconciliation

Each thread-local `Encoder` assigns pool IDs starting from
0. When the flush thread transcodes a batch, the
`Transcoder` decodes string pool frames into the decoder's
`StringPool` (id → string). When re-encoding events that
reference interned strings, the `Transcoder` looks up the
string by local ID in the batch's pool, then calls
`target_encoder.intern_string(s)` to get the global ID.
This is the same approach used by `Decoder::into_encoder`.

### Timestamp handling

Per-thread encoders encode timestamps as deltas from their
own base. The `Transcoder` decodes these back to absolute
timestamps (the `Decoder` reconstructs absolute values),
then the target `Encoder` re-encodes them as deltas from
the file's timestamp base. No precision is lost.

### CPU profiling events

CPU sample events (`CpuSample`) are produced by the
`CpuProfiler` and `SchedProfiler`, which are drained on the
flush thread via `EventWriter::flush_cpu`. These events
bypass the thread-local encoding path entirely — they are
already produced on the flush thread and written directly
through the central encoder. No change needed.

### `TraceWriter` trait

The `TraceWriter` trait currently accepts `&RawEvent`. After
this change, the flush thread handles both `RawEvent`s (via
the existing path) and encoded bytes (via the
`Transcoder`). Two options:

**Option A (recommended)**: Add a `write_transcoded_batch`
method to `EventWriter` that accepts `&[u8]` and transcodes
through the central writer using the `Transcoder`. The
`TraceWriter` trait itself does not change —
`RotatingWriter` gains an internal method for writing from
transcoded data.

**Option B**: Change `TraceWriter` to accept encoded bytes
directly. This is a larger refactor and breaks the
`CapturingWriter` test helper.

Option A is preferred because it is additive and does not
change the `TraceWriter` trait signature.

## Code Architecture / File Changes

### `dial9-trace-format` changes

| File | Change |
|------|--------|
| `encoder.rs` | Add `reset_to(&mut self, W) -> W` and `reset(&mut self) -> Vec<u8>` methods. Add `write_event_ref(&mut self, &Schema, u64, &[FieldValueRef])` for zero-copy transcoding. |
| `types.rs` | Add `write_field_value_ref` to `EventEncoder`. Add `raw_data()` to `StackFramesRef` and `StringMapRef`. |
| `transcoder.rs` (new) | Free function `transcode(&[u8], &mut Encoder<W>)` + `TranscodeError` enum. |
| `decoder.rs` | Add `try_for_each_event` with fallible callback. Add `pub(crate)` accessors: `pos()`, `advance()`, `timestamp_base_ns()`, `set_timestamp_base()`, `schema_info()`. Add `TryForEachError<E>` enum. |
| `lib.rs` | Export `pub mod transcoder`. |

### `dial9-tokio-telemetry` changes

| File | Change |
|------|--------|
| `buffer.rs` | Add `Encoder<Vec<u8>>` path alongside `Vec<RawEvent>`. Change `flush` to return `Batch` struct with both `raw_events` and `encoded_bytes`. Add `should_encode` routing and `encode_event`. Cache formatted `String`s (not `InternedString`s) in `location_cache`. |
| `collector.rs` | Add `Batch` struct (`raw_events: Vec<RawEvent>`, `encoded_bytes: Vec<u8>`). Change `ArrayQueue<Vec<RawEvent>>` to `ArrayQueue<Batch>`. Single `accept_flush(Batch)` method. |
| `events.rs` | No change to `RawEvent` enum (still used by CPU profiler path and legacy dual-path). |
| `recorder/mod.rs` | Update `flush_once` to process both `batch.raw_events` and `batch.encoded_bytes`, calling `transcoder::transcode` for the encoded portion. |
| `recorder/event_writer.rs` | Add `write_transcoded_batch` method that calls `transcoder::transcode` through the central writer. |
| `writer.rs` | No change — `RotatingWriter` continues to own the central `Encoder`. |
| `recorder/shared_state.rs` | `record_event` still creates `RawEvent` and passes it to `buffer::record_event`. No change to the hot-path API. |
| `format.rs` | No change — wire format event structs unchanged. |

### New types

- `Batch` struct in `collector.rs`: contains both
  `raw_events: Vec<RawEvent>` and
  `encoded_bytes: Vec<u8>`.
- `TranscodeError` in `dial9-trace-format/src/transcoder.rs`.
- `TryForEachError<E>` in `dial9-trace-format/src/decoder.rs`.

### Removed types

None. `RawEvent` is preserved for the dual-path
architecture and CPU profiler events.

## Testing Strategy

### Unit tests

- **`encoder.rs`**: Test `reset_to` preserves allocations
  (capacity of internal maps does not shrink) and produces
  a valid header on the new writer. Test `reset` returns
  decodable bytes.
- **`transcoder.rs`**: Test round-trip (encode → transcode
  → decode yields identical events). Test string pool
  remapping (same string from two batches gets the same
  global ID). Test timestamp rebasing (events from batches
  with different bases produce correct absolute timestamps).
  Test empty batch (header only, no events).
- **`buffer.rs`**: Test that `record_event` followed by
  `flush` produces a `Batch` whose `encoded_bytes` decode
  to the expected events (round-trip through
  `Decoder::for_each_event`), and whose `raw_events`
  contain the not-yet-migrated events.
- **`collector.rs`**: Test `Batch` flow through
  `ArrayQueue` (accept, drain, eviction). Test that
  batches with only `raw_events`, only `encoded_bytes`,
  or both are handled correctly.

### Integration tests

- **`end_to_end.rs`**: Existing end-to-end tests already
  validate that traces contain the expected events. These
  tests exercise the full pipeline and will catch
  regressions without modification.
- **`validation.rs`**: Validates trace-matches-metrics
  invariant — unchanged.

### Stress tests

- Run `cargo nextest run --stress-duration 20` to verify no
  flaky tests introduced.
- The existing `s3_stress_test` exercises high event rates
  and will surface any transcoding bugs.

## Implementation Order

1. **MVP benchmark** (pre-work): Before any implementation,
   create a benchmark that measures the end-to-end cost of
   the thread-local encode → transcode → central encode
   pipeline vs. the current `RawEvent` → `write_resolved`
   path. Use the existing `writer_encode` bench as a
   starting point. This establishes a performance baseline
   and validates that the approach does not regress
   throughput. The benchmark should:
   - Encode a batch of `RawEvent`s through a thread-local
     `Encoder<Vec<u8>>`.
   - Transcode the resulting bytes through a central
     `Encoder<Vec<u8>>`.
   - Compare throughput (events/sec) and per-event latency
     against the current direct-encode path.

2. **Add `Encoder::reset_to`** to `dial9-trace-format`.
   Unit test that it preserves allocations and produces
   valid output.

3. **Add `transcode` free function** to
   `dial9-trace-format` as a first-class module. Add
   `Encoder::write_event_ref` for zero-copy re-encoding
   with `&[FieldValueRef]`. Add `try_for_each_event` to
   `Decoder`. Unit test round-trip transcoding with string
   pool remapping and timestamp rebasing.

4. **Add `Batch` struct** to `collector.rs`. Change
   `ArrayQueue<Vec<RawEvent>>` to `ArrayQueue<Batch>`.
   Initially, `encoded_bytes` is always empty and
   `raw_events` carries all events — existing behavior
   is preserved.

5. **Add thread-local encoding** in `buffer.rs`: add
   `Encoder<Vec<u8>>` alongside `Vec<RawEvent>`, implement
   `should_encode` routing and `encode_event`, produce
   `Batch` with both halves on flush. Use `reset_to`
   instead of creating a new encoder.

6. **Wire it together** in `flush_once`: process
   `batch.raw_events` through the existing path and
   `batch.encoded_bytes` through `transcoder::transcode`.

7. **Benchmark**: run the MVP benchmark from step 1 to
   validate performance. Run `compare_overhead.sh` to
   measure end-to-end impact.

## Open Questions

1. **Location cache lifetime**: The `location_cache` in
   `ThreadLocalBuffer` maps `Location<'static>` →
   `String` (the formatted location text). Because it
   caches the formatted `String` rather than
   `InternedString` IDs, it survives encoder resets — only
   the `intern_string` call is repeated, avoiding the
   `Location::to_string()` formatting cost. The number of
   distinct spawn locations is small (one per
   `tokio::spawn` call site), so the cache stays small.

2. **Batch header overhead**: Each encoded batch contains
   a 5-byte file header, schema frames, and string pool
   frames in addition to event data. For 1024 events this
   overhead is negligible (~0.5%), but if `BUFFER_CAPACITY`
   is reduced, it becomes proportionally larger. We could
   strip headers during transcoding or use a headerless
   encoding mode for thread-local buffers.

3. **CPU profiler events**: Currently these bypass the
   thread-local path. If CPU profiling generates high event
   rates, we may want to encode them on the sampler thread
   too. This is out of scope for the initial
   implementation.

4. **Dual-path sunset**: The `raw_events` path in `Batch`
   is retained indefinitely for benchmarking and
   incremental migration. No timeline for removing it.
