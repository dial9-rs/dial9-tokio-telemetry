# Task 03: Transcoder and try_for_each_event

Agent: implementer
Design: docs/design/threadlocal-encoding.md
Depends on: Task 02

## Objective

Add a `transcode` free function to `dial9-trace-format`
that decodes a byte stream from one encoder and re-encodes
it through a target encoder, handling string pool remapping,
timestamp rebasing, and schema deduplication. Also add
`try_for_each_event` to `Decoder` for fallible callbacks,
and `write_event_ref` to `Encoder` for zero-copy
re-encoding.

## Target Tests

- `test_transcode_round_trip`
- `test_transcode_string_pool_remapping`
- `test_transcode_timestamp_rebasing`
- `test_transcode_empty_batch`

## Implementation

### New encoder APIs

File: `dial9-trace-format/src/encoder.rs`

Add `write_event_ref` to `Encoder<W>`:

```rust
pub fn write_event_ref(
    &mut self,
    schema: &Schema,
    timestamp_ns: u64,
    values: &[FieldValueRef<'_>],
) -> io::Result<()>
```

Unlike `write_event`, the timestamp is a separate argument
(not extracted from values[0]) since the decoder provides
it directly.

File: `dial9-trace-format/src/types.rs`

Add `write_field_value_ref` to `EventEncoder`:

```rust
pub fn write_field_value_ref(
    &mut self,
    value: &FieldValueRef<'_>,
) -> io::Result<()>
```

For `StackFrames` and `StringMap`, write raw backing bytes
via new `raw_data()` accessors on `StackFramesRef` and
`StringMapRef`.

### `try_for_each_event` on `Decoder`

File: `dial9-trace-format/src/decoder.rs`

```rust
pub fn try_for_each_event<E>(
    &mut self,
    mut f: impl for<'f> FnMut(RawEvent<'a, 'f>)
        -> Result<(), E>,
) -> Result<(), TryForEachError<E>>
```

Also add `pub(crate)` accessors: `pos()`, `advance()`,
`timestamp_base_ns()`, `set_timestamp_base()`,
`schema_info()`.

### `transcode` free function

File: `dial9-trace-format/src/transcoder.rs` (new)

```rust
pub fn transcode<W: Write>(
    source: &[u8],
    target: &mut Encoder<W>,
) -> Result<(), TranscodeError>
```

This is a **free function**, not a struct. The `values_buf`
must be local to the function because `FieldValueRef<'a>`
borrows from `source` — it cannot live on a struct across
calls.

The function manually iterates decoder frames (not using
`try_for_each_event`) because it needs simultaneous access
to:
1. The decoder (string pool, timestamp base)
2. A `HashMap<WireTypeId, Schema>` built as schema frames
   are encountered
3. The target encoder

For each event:
1. Decode fields into local `values_buf: Vec<FieldValueRef>`
2. Remap `PooledString` IDs: resolve via decoder's pool,
   re-intern through `target.intern_string()`
3. Call `target.write_event_ref(schema, timestamp, &values)`

### Key design decisions

- **Free function, not struct**: No useful state to carry
  across calls. `values_buf` borrows from `source` so it
  can't live on a struct. A `Vec<FieldValue>` (owned) would
  force copying every string/bytes field — defeating
  zero-copy.
- **Manual frame iteration**: `try_for_each_event` mutably
  borrows the decoder for the entire iteration, preventing
  access to the schema registry from inside the callback.
- **`write_event_ref`**: Avoids converting `FieldValueRef`
  → `FieldValue` (which would copy strings and bytes).
- **`raw_data()` on StackFramesRef/StringMapRef**: Enables
  true zero-copy for variable-length types.

## Acceptance Criteria

- All target tests pass.
- Existing decoder and encoder tests still pass.
- `cargo clippy --all-targets --all-features` is clean.
