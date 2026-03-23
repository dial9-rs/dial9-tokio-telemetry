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

## Technical Design

### Dual-path architecture

Both the `RawEvent` enum path and the encoded-bytes path
will coexist. This enables:

1. **Incremental migration**: existing event types can move
   to thread-local encoding one at a time.
2. **Benchmarking**: the `RawEvent` path serves as a
   baseline for measuring the performance impact of
   thread-local encoding.
3. **CPU profiler events**: `CpuSample` events are produced
   on the flush thread and written directly through the
   central encoder — they never touch the thread-local path.

The `CentralCollector` accepts both batch types:

```rust
// collector.rs
pub(crate) enum Batch {
    /// Legacy path: unencoded RawEvents.
    Raw(Vec<RawEvent>),
    /// New path: pre-encoded byte stream.
    Encoded(EncodedBatch),
}

pub(crate) struct EncodedBatch {
    /// Complete dial9-trace-format byte stream (header +
    /// schemas + string pools + events) from one thread's
    /// encoding session.
    pub bytes: Vec<u8>,
}
```

The flush thread handles both:

```rust
// flush_once (sketch)
while let Some(batch) = shared.collector.next() {
    match batch {
        Batch::Raw(events) => {
            for raw in events {
                event_writer.write_raw_event(raw)?;
            }
        }
        Batch::Encoded(batch) => {
            transcoder.transcode(
                &batch.bytes,
                &mut event_writer,
            )?;
        }
    }
}
```

### Phase 1: Thread-local `Encoder<Vec<u8>>`

Replace `Vec<RawEvent>` in `ThreadLocalBuffer` with an
`Encoder<Vec<u8>>` that encodes events into a byte buffer.

```rust
// buffer.rs (new)
pub(crate) struct ThreadLocalBuffer {
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

`record_event` encodes immediately:

```rust
fn record_event(&mut self, event: RawEvent) {
    self.encode_event(&event);
    self.event_count += 1;
}
```

The `encode_event` method mirrors the current
`RotatingWriter::write_resolved` logic but writes to the
in-memory `Vec<u8>` encoder. String interning uses the
thread-local encoder's pool — each thread assigns its own
pool IDs independently.

On flush (when `event_count >= BUFFER_CAPACITY`), the
buffer extracts the encoded bytes and resets the encoder
into a fresh `Vec<u8>` using `reset_to`:

```rust
fn flush(&mut self) -> EncodedBatch {
    let bytes = self.encoder.reset_to(
        Vec::with_capacity(ESTIMATED_BATCH_SIZE)
    );
    self.event_count = 0;
    // location_cache is preserved — it caches formatted
    // Strings, not InternedString IDs, so it survives
    // encoder resets.
    EncodedBatch { bytes }
}
```

### Phase 2: `EncodedBatch` in `CentralCollector`

`CentralCollector` changes from
`ArrayQueue<Vec<RawEvent>>` to `ArrayQueue<Batch>`:

```rust
pub fn accept_flush_encoded(&self, batch: EncodedBatch) {
    if let Some(_evicted) =
        self.queue.force_push(Batch::Encoded(batch))
    {
        self.dropped_batches
            .fetch_add(1, Ordering::Relaxed);
    }
}
```

The existing `accept_flush(Vec<RawEvent>)` wraps in
`Batch::Raw` for backward compatibility.

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

```rust
// dial9-trace-format/src/transcoder.rs

/// Transcode a complete dial9-trace-format byte stream
/// through a target encoder. Handles:
/// - Schema deduplication and type ID remapping
/// - String pool ID remapping
/// - Timestamp rebasing (decode to absolute, re-encode
///   as delta from target encoder's base)
pub struct Transcoder {
    /// Reusable decode buffer for field values.
    values_buf: Vec<FieldValueRef<'static>>,
}

impl Transcoder {
    pub fn new() -> Self {
        Self {
            values_buf: Vec::new(),
        }
    }

    /// Transcode all events from `source` bytes into
    /// `target` encoder.
    pub fn transcode<W: Write>(
        &mut self,
        source: &[u8],
        target: &mut Encoder<W>,
    ) -> Result<(), TranscodeError> {
        let mut decoder = Decoder::new(source)
            .ok_or(TranscodeError::InvalidHeader)?;
        decoder.for_each_event(|ev| {
            // Re-intern pooled strings through target
            // encoder. Remap field values containing
            // InternedString references.
            // Re-encode event through target encoder's
            // write_event, which handles timestamp
            // delta encoding and schema registration.
        })?;
        Ok(())
    }
}

#[derive(Debug)]
pub enum TranscodeError {
    InvalidHeader,
    Decode(DecodeError),
    Io(io::Error),
}
```

The `Transcoder` is stateful to allow reusing allocations
across batches. The telemetry crate's flush thread creates
one `Transcoder` and reuses it for every `EncodedBatch`.

The `for_each_event` callback receives
`dial9_trace_format::decoder::RawEvent` refs whose fields
contain `FieldValueRef::PooledString(InternedString)`. The
decoder's `StringPool` resolves these to `&str`, and the
target encoder re-interns them via `intern_string`.

Note: `for_each_event`'s callback returns `()`, so I/O
errors from re-encoding must be captured in a local
variable and checked after iteration. A
`try_for_each_event` method that accepts `FnMut → Result`
should be added alongside the `Transcoder` to support
fallible callbacks cleanly.

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
the existing path) and `EncodedBatch`es (via the
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
| `encoder.rs` | Add `reset_to(&mut self, W) -> W` and `reset(&mut self) -> Vec<u8>` methods. |
| `transcoder.rs` (new) | `Transcoder` struct with `transcode(&mut self, &[u8], &mut Encoder<W>)`. |
| `decoder.rs` | Add `try_for_each_event` with fallible callback (`FnMut → Result`). |
| `lib.rs` | Export `pub mod transcoder`. |

### `dial9-tokio-telemetry` changes

| File | Change |
|------|--------|
| `buffer.rs` | Add `Encoder<Vec<u8>>` path alongside `Vec<RawEvent>`. Change `flush` to return `Batch::Encoded` when using the encoded path. Cache formatted `String`s (not `InternedString`s) in `location_cache`. |
| `collector.rs` | Add `Batch` enum (`Raw`/`Encoded`). Add `EncodedBatch` struct. Change `ArrayQueue<Vec<RawEvent>>` to `ArrayQueue<Batch>`. Add `accept_flush_encoded`. |
| `events.rs` | No change to `RawEvent` enum (still used by CPU profiler path and legacy dual-path). |
| `recorder/mod.rs` | Update `flush_once` to match on `Batch::Raw` / `Batch::Encoded`, calling `transcode` for encoded batches. |
| `recorder/event_writer.rs` | Add `write_transcoded_batch` method that uses `Transcoder` to re-encode through the central writer. |
| `writer.rs` | No change — `RotatingWriter` continues to own the central `Encoder`. |
| `recorder/shared_state.rs` | `record_event` still creates `RawEvent` and passes it to `buffer::record_event`. No change to the hot-path API. |
| `format.rs` | No change — wire format event structs unchanged. |

### New types

- `Batch` enum in `collector.rs`.
- `EncodedBatch` in `collector.rs`: wraps `Vec<u8>`.
- `Transcoder` in `dial9-trace-format/src/transcoder.rs`.
- `TranscodeError` in `dial9-trace-format/src/transcoder.rs`.

### Removed types

None. `RawEvent` is preserved for the dual-path
architecture and CPU profiler events.

## Testing Strategy

### Unit tests

- **`encoder.rs`**: Test `reset_to` preserves allocations
  (capacity of internal maps does not shrink) and produces
  a valid header on the new writer. Test `reset` returns
  decodable bytes.
- **`transcoder.rs`**: Test that a batch encoded by one
  encoder, when transcoded through a `Transcoder` into a
  second encoder, produces identical decoded events. Test
  string pool remapping (same string from two batches gets
  the same global ID). Test timestamp rebasing (events from
  batches with different bases produce correct absolute
  timestamps in the output).
- **`buffer.rs`**: Test that `record_event` followed by
  `flush` produces an `EncodedBatch` whose bytes decode to
  the expected events (round-trip through
  `Decoder::for_each_event`).
- **`collector.rs`**: Test `Batch::Encoded` flow through
  `ArrayQueue` (accept, drain, eviction). Test
  `Batch::Raw` backward compatibility.

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

3. **Add `Transcoder`** to `dial9-trace-format` as a
   first-class module. Add `try_for_each_event` to
   `Decoder`. Unit test round-trip transcoding with string
   pool remapping and timestamp rebasing.

4. **Add `Batch` enum and `EncodedBatch`** to
   `collector.rs`. Keep the old `Vec<RawEvent>` path
   working via `Batch::Raw`.

5. **Add thread-local encoding** in `buffer.rs`: add
   `Encoder<Vec<u8>>` alongside `Vec<RawEvent>`, implement
   `encode_event`, produce `EncodedBatch` on flush. Use
   `reset_to` instead of creating a new encoder.

6. **Wire it together** in `flush_once`: match on
   `Batch::Raw` / `Batch::Encoded`, calling `Transcoder`
   for encoded batches.

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

2. **Batch header overhead**: Each `EncodedBatch` contains
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

4. **Dual-path sunset**: The `Batch::Raw` path is retained
   indefinitely for benchmarking and incremental migration.
   No timeline for removing it.
