# Task 03: Transcoder and try_for_each_event

Agent: implementer
Design: docs/design/threadlocal-encoding.md
Depends on: Task 02

## Objective

Add a `Transcoder` to `dial9-trace-format` that decodes a
byte stream from one encoder and re-encodes it through a
target encoder, handling string pool remapping, timestamp
rebasing, and schema deduplication. Also add
`try_for_each_event` to `Decoder` for fallible callbacks.

## Target Tests

- `test_transcode_round_trip_single_batch`
- `test_transcode_string_pool_remapping`
- `test_transcode_timestamp_rebasing`
- `test_transcode_schema_deduplication`
- `test_transcode_empty_batch`
- `test_try_for_each_event_propagates_error`
- `test_try_for_each_event_success`

## Implementation

### `try_for_each_event` on `Decoder`

File: `dial9-trace-format/src/decoder.rs`

Add a method alongside the existing `for_each_event`:

```rust
pub fn try_for_each_event<E>(
    &mut self,
    mut f: impl for<'f> FnMut(RawEvent<'a, 'f>) -> Result<(), E>,
) -> Result<(), TryForEachError<E>>
```

where `TryForEachError` wraps either a `DecodeError` or
the user's error `E`. The implementation mirrors
`for_each_event` but calls `f(event)?` instead of
`f(event)`.

### `Transcoder`

File: `dial9-trace-format/src/transcoder.rs` (new)

```rust
pub struct Transcoder {
    values_buf: Vec<FieldValueRef<'static>>,
}
```

Method `transcode<W: Write>(&mut self, source: &[u8],
target: &mut Encoder<W>) -> Result<(), TranscodeError>`:

1. Create a `Decoder` from `source`.
2. Call `decoder.try_for_each_event(|ev| { ... })`.
3. For each event:
   a. Resolve any `FieldValueRef::PooledString` values
      using the decoder's `StringPool` to get `&str`.
   b. Re-intern each string through
      `target.intern_string(s)`.
   c. Build a `Vec<FieldValue>` with remapped pool IDs.
   d. Call `target.write_event(schema, timestamp, &fields)`
      to re-encode. The target encoder handles timestamp
      delta encoding and schema registration.

The `Transcoder` reuses `values_buf` across calls to avoid
per-batch allocation.

Export: add `pub mod transcoder;` to
`dial9-trace-format/src/lib.rs`.

### Key design decisions

- The `Transcoder` does not need to explicitly remap schema
  type IDs — `target.write_event` with the schema handle
  does this automatically via `ensure_registered`.
- String remapping happens per-event: decode the local pool
  ID → look up string in decoder's pool → intern through
  target encoder → get global pool ID.
- Timestamps: the `Decoder` reconstructs absolute
  timestamps. Passing them to `target.write_event` lets the
  target encoder compute its own deltas.

## Test Requirements

Unit tests in `dial9-trace-format/src/transcoder.rs`
`#[cfg(test)]` module covering the same scenarios as the
E2E tests but at the unit level.

## Acceptance Criteria

- All target tests from Task 01 pass.
- Existing decoder and encoder tests still pass.
- `cargo clippy --all-targets --all-features` is clean.
