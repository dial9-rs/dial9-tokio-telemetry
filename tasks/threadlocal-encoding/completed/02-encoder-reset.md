# Task 02: Encoder::reset_to

Agent: implementer
Design: docs/design/threadlocal-encoding.md

## Objective

Add `reset_to` and `reset` methods to `Encoder` in
`dial9-trace-format` so thread-local encoders can be
reused across flush cycles without reallocating internal
maps.

## Target Tests

- `test_reset_to_returns_decodable_bytes`
- `test_reset_convenience_returns_decodable_bytes`

## Implementation

File: `dial9-trace-format/src/encoder.rs`

Add to `impl<W: Write> Encoder<W>`:

```rust
pub fn reset_to(&mut self, new_writer: W) -> W
```

This method:
1. Writes a file header to `new_writer` via
   `codec::encode_header`.
2. Clears `self.string_pool` in place (preserving
   allocation).
3. Resets `self.next_pool_id` to 0.
4. Clears `self.registry.schemas` in place (preserving
   allocation) and resets `self.registry.next_id` to 0.
5. Clears `self.schema_ids` in place.
6. Swaps `self.state` with a new `EncodeState` wrapping
   `new_writer`.
7. Returns the old writer via
   `old_state.writer.into_inner()`.

Add to `impl Encoder<Vec<u8>>`:

```rust
pub fn reset(&mut self, sz: usize) -> Vec<u8> {
    self.reset_to(Vec::with_capacity(sz))
}
```

The `SchemaRegistry` fields (`schemas`, `next_id`) are
`pub(crate)`, so direct access from `Encoder` within the
same crate is fine. Verify this by checking
`dial9-trace-format/src/schema.rs`.

## Test Requirements

Unit tests in the existing `encoder.rs` `#[cfg(test)]`
module:
- Verify `reset_to` returns bytes that decode to the
  expected events.
- Verify the encoder produces valid output after reset
  (encode → reset → encode → decode second session).
- Verify `string_pool` capacity does not shrink after
  `reset_to` (use `HashMap::capacity()`).

## Acceptance Criteria

- Target tests from Task 01 pass.
- Existing encoder tests still pass.
- `cargo clippy --all-targets --all-features` is clean.
