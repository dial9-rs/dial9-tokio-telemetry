# Thread-Local Encoding with Central Transcoding

## Overview

Move event encoding from the flush thread into per-worker
thread-local buffers. Each worker thread encodes `RawEvent`s
into wire-format bytes locally, then flushes pre-encoded byte
buffers to the `CentralCollector`. The flush thread transcodes
(reconciles string pools, rebases timestamps) and writes the
final bytes to disk.

This eliminates the flush thread as a serialization bottleneck
and distributes encoding work across all worker threads, where
it can overlap with I/O waits (park/unpark).

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

### Bottleneck

All encoding work runs on the single `dial9-flush` thread.
With high event rates (e.g. 8 workers × 200k polls/sec), the
flush thread becomes CPU-bound on encoding, causing the
`CentralCollector` to back up and drop batches. Encoding is
pure computation (varint packing, string hashing, delta
arithmetic) that can be parallelized across worker threads.

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

## Technical Design

### Phase 1: Thread-local `Encoder<Vec<u8>>`

Replace `Vec<RawEvent>` in `ThreadLocalBuffer` with an
`Encoder<Vec<u8>>` that encodes events into a byte buffer.

```rust
// buffer.rs (new)
pub(crate) struct ThreadLocalBuffer {
    encoder: Encoder<Vec<u8>>,
    event_count: usize,
    collector: Option<Arc<CentralCollector>>,
    /// Cache: Location → interned string ID, avoids
    /// reformatting the same spawn location on every
    /// PollStart. Uses the Location value (which impls
    /// Hash + Eq) as key, matching the existing
    /// RotatingWriter::formatted_locations pattern.
    location_cache: HashMap<
        std::panic::Location<'static>,
        InternedString,
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
buffer extracts the encoded bytes and resets:

```rust
fn flush(&mut self) -> EncodedBatch {
    let bytes = self.encoder.finish();
    self.encoder = Encoder::new();
    self.event_count = 0;
    // location_cache must be cleared because the
    // InternedString IDs are local to the old encoder.
    // The new encoder starts with a fresh string pool.
    self.location_cache.clear();
    EncodedBatch { bytes }
}
```

### Phase 2: `EncodedBatch` replaces `Vec<RawEvent>`

```rust
// collector.rs (new)
pub(crate) struct EncodedBatch {
    /// Complete dial9-trace-format byte stream (header +
    /// schemas + string pools + events) from one thread's
    /// encoding session.
    pub bytes: Vec<u8>,
}
```

`CentralCollector` changes from
`ArrayQueue<Vec<RawEvent>>` to
`ArrayQueue<EncodedBatch>`.

### Phase 3: Flush-thread transcoding

The flush thread drains `EncodedBatch`es and transcodes
them into the central `RotatingWriter`. Transcoding means:

1. **Decode** each batch using `Decoder` (zero-copy ref
   decoding via `for_each_event`).
2. **Remap string pool IDs**: the batch's local pool IDs
   are re-interned through the central `Encoder`'s
   `intern_string`, which deduplicates and assigns global
   IDs.
3. **Re-encode events**: for each decoded event, write it
   through the central `Encoder::write` which handles
   timestamp delta encoding relative to the file's base.
4. **Schema deduplication**: `Encoder::write<T>` already
   deduplicates schemas via `ensure_registered`. Decoded
   schemas from the batch are re-registered on the central
   encoder; duplicates are no-ops.

The existing `Decoder::into_encoder` method provides a
foundation, but the flush path needs a lighter approach:
decode each batch's frames and re-emit them through the
central encoder, rather than creating a new encoder per
batch.

```rust
// flush_once (sketch)
fn flush_once(...) -> FlushStats {
    // ... drain CPU profilers as before ...
    buffer::drain_to_collector(&shared.collector);

    while let Some(batch) = shared.collector.next() {
        transcode_batch(
            &batch.bytes,
            &mut event_writer,
        )?;
    }
    event_writer.flush()?;
}
```

The `transcode_batch` function:

```rust
fn transcode_batch(
    bytes: &[u8],
    writer: &mut EventWriter,
) -> io::Result<()> {
    let mut decoder = Decoder::new(bytes)
        .ok_or_else(|| io::Error::new(
            io::ErrorKind::InvalidData,
            "invalid batch header",
        ))?;
    let mut err: Option<io::Error> = None;
    decoder.for_each_event(|ev| {
        if err.is_some() {
            return;
        }
        if let Err(e) = writer.write_decoded_event(ev) {
            err = Some(e);
        }
    })?;
    match err {
        Some(e) => Err(e),
        None => Ok(()),
    }
}
```

Note: `for_each_event`'s callback returns `()`, so I/O
errors from `write_decoded_event` must be captured in a
local variable and checked after iteration. An alternative
is to add a fallible `try_for_each_event` method to
`Decoder` in `dial9-trace-format`.

### String pool reconciliation

Each thread-local `Encoder` assigns pool IDs starting from
0. When the flush thread decodes a batch, string pool frames
produce a local `StringPool` (id → string). When re-encoding
events that reference interned strings, the flush thread
looks up the string by local ID in the batch's pool, then
calls `central_encoder.intern_string(s)` to get the global
ID. This is already how `Decoder::into_encoder` works.

The `for_each_event` callback receives
`dial9_trace_format::decoder::RawEvent` refs (not to be
confused with the telemetry `RawEvent` enum) whose fields
contain `FieldValueRef::PooledString(InternedString)`. The
decoder's `StringPool` resolves these to `&str`, and the
central encoder re-interns them.

### Timestamp handling

Per-thread encoders encode timestamps as deltas from their
own base. The flush thread decodes these back to absolute
timestamps (the `Decoder` reconstructs absolute values), then
the central `Encoder` re-encodes them as deltas from the
file's timestamp base. No precision is lost.

### CPU profiling events

CPU sample events (`CpuSample`) are produced by the
`CpuProfiler` and `SchedProfiler`, which are drained on the
flush thread via `EventWriter::flush_cpu`. These events
bypass the thread-local encoding path entirely — they are
already produced on the flush thread and written directly
through the central encoder. No change needed.

### `TraceWriter` trait

The `TraceWriter` trait currently accepts `&RawEvent`. After
this change, the flush thread no longer has `RawEvent`s — it
has decoded event refs from batches. Two options:

**Option A (recommended)**: Add a `write_decoded_event`
method to `EventWriter` that accepts decoded frame refs and
re-encodes them through the central writer. The `TraceWriter`
trait itself does not change — `RotatingWriter` gains an
internal method for writing from decoded refs.

**Option B**: Change `TraceWriter` to accept encoded bytes
directly. This is a larger refactor and breaks the
`CapturingWriter` test helper.

Option A is preferred because it is additive and does not
change the `TraceWriter` trait signature.

## Code Architecture / File Changes

| File | Change |
|------|--------|
| `buffer.rs` | Replace `Vec<RawEvent>` with `Encoder<Vec<u8>>` + `location_cache`. Change `record_event` to encode immediately. Change `flush` to return `EncodedBatch`. |
| `collector.rs` | Change `ArrayQueue<Vec<RawEvent>>` to `ArrayQueue<EncodedBatch>`. Update `accept_flush` and `next` signatures. |
| `events.rs` | No change to `RawEvent` enum (still used by CPU profiler path). |
| `recorder/mod.rs` | Update `flush_once` to call `transcode_batch` instead of iterating `RawEvent`s. |
| `recorder/event_writer.rs` | Add `write_decoded_event` method that re-encodes a decoded event ref through the central writer. |
| `writer.rs` | Add `write_decoded_ref` to `RotatingWriter` that accepts decoded event data and re-encodes through the central `Encoder`. |
| `recorder/shared_state.rs` | `record_event` still creates `RawEvent` and passes it to `buffer::record_event`. No change to the hot-path API. |
| `format.rs` | No change — wire format event structs unchanged. |

### New types

- `EncodedBatch` in `collector.rs`: wraps `Vec<u8>`.
- `transcode_batch` function in `recorder/mod.rs` or a new
  `transcoder.rs` module.

### Removed types

None. `RawEvent` is still used for CPU profiler events.

## Testing Strategy

### Unit tests

- **`buffer.rs`**: Test that `record_event` followed by
  `flush` produces an `EncodedBatch` whose bytes decode to
  the expected events (round-trip through
  `Decoder::for_each_event`).
- **`collector.rs`**: Test `EncodedBatch` flow through
  `ArrayQueue` (accept, drain, eviction).
- **`transcode_batch`**: Test that a batch encoded by a
  thread-local encoder, when transcoded through a central
  encoder, produces identical decoded events. Test string
  pool remapping (same string from two batches gets the same
  global ID). Test timestamp rebasing (events from batches
  with different bases produce correct absolute timestamps
  in the output).

### Integration tests

- **`end_to_end.rs`**: Existing end-to-end tests already
  validate that traces contain the expected events. These
  tests exercise the full pipeline and will catch regressions
  without modification.
- **`validation.rs`**: Validates trace-matches-metrics
  invariant — unchanged.
- **`writer_encode` bench**: Update to use the new path.
  Compare throughput before/after to validate the
  performance improvement.

### Stress tests

- Run `cargo nextest run --stress-duration 20` to verify no
  flaky tests introduced.
- The existing `s3_stress_test` exercises high event rates
  and will surface any transcoding bugs.

## Implementation Order

1. **Add `EncodedBatch` type** to `collector.rs`. Keep the
   old `Vec<RawEvent>` path working alongside it (feature
   flag or enum).
2. **Add thread-local encoding** in `buffer.rs`: replace
   `Vec<RawEvent>` with `Encoder<Vec<u8>>`, implement
   `encode_event`, produce `EncodedBatch` on flush.
3. **Add `transcode_batch`** and `write_decoded_event`:
   implement the flush-thread decode-and-re-encode path.
4. **Wire it together** in `flush_once`: switch from
   iterating `Vec<RawEvent>` to transcoding
   `EncodedBatch`es.
5. **Remove dead code**: drop the `RawEvent` variants from
   `ThreadLocalBuffer` (keep `RawEvent` for CPU profiler).
6. **Benchmark**: run `writer_encode` bench and
   `compare_overhead.sh` to measure improvement.

## Open Questions

1. **Location cache lifetime**: The `location_cache` in
   `ThreadLocalBuffer` maps `Location<'static>` → 
   `InternedString`. It must be cleared on every flush
   because `InternedString` IDs are local to the encoder
   that created them. This means spawn locations are
   re-formatted and re-interned every 1024 events. In
   practice, the number of distinct spawn locations is
   small (one per `tokio::spawn` call site), so this cost
   is negligible. An optimization would be to cache the
   formatted `String` (not the `InternedString`) so only
   the `intern_string` call is repeated, avoiding the
   `Location::to_string()` formatting.

2. **Encoder reset cost**: Creating a new `Encoder<Vec<u8>>`
   on every flush allocates a new `Vec`, `HashMap`, and
   `SchemaRegistry`. An alternative is to add an
   `Encoder::reset` method that clears internal state and
   reuses allocations. This is a `dial9-trace-format` API
   addition.

3. **Batch header overhead**: Each `EncodedBatch` contains a
   5-byte file header, schema frames, and string pool frames
   in addition to event data. For 1024 events this overhead
   is negligible (~0.5%), but if `BUFFER_CAPACITY` is
   reduced, it becomes proportionally larger. We could strip
   headers during transcoding or use a headerless encoding
   mode for thread-local buffers.

4. **CPU profiler events**: Currently these bypass the
   thread-local path. If CPU profiling generates high event
   rates, we may want to encode them on the sampler thread
   too. This is out of scope for the initial implementation.
