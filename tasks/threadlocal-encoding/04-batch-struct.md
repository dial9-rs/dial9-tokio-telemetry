# Task 04: Batch Struct in CentralCollector

Agent: implementer
Design: docs/design/threadlocal-encoding.md

## Objective

Introduce a `Batch` struct in `collector.rs` and change
`CentralCollector` from `ArrayQueue<Vec<RawEvent>>` to
`ArrayQueue<Batch>`. Initially `encoded_bytes` is always
empty — existing behavior is preserved.

## Target Tests

No Task 01 tests target this directly. This is a
refactoring task validated by existing tests continuing
to pass.

## Implementation

File: `dial9-tokio-telemetry/src/telemetry/collector.rs`

1. Add the `Batch` struct:

```rust
pub(crate) struct Batch {
    pub raw_events: Vec<RawEvent>,
    pub encoded_bytes: Vec<u8>,
}
```

2. Change `ArrayQueue<Vec<RawEvent>>` to
   `ArrayQueue<Batch>`.

3. Change `accept_flush` to accept `Batch`:

```rust
pub fn accept_flush(&self, batch: Batch) { ... }
```

4. Change `next` to return `Option<Batch>`.

5. Update all call sites:

   - `buffer.rs`: `record_event` and `drain_to_collector`
     currently call `collector.accept_flush(vec)`. Wrap in
     `Batch { raw_events: vec, encoded_bytes: Vec::new() }`.
   - `buffer.rs` `Drop` impl: same wrapping.
   - `recorder/mod.rs` `flush_once`: currently iterates
     `while let Some(batch) = shared.collector.next()` and
     treats the result as `Vec<RawEvent>`. Change to
     destructure `Batch` and iterate `batch.raw_events`.

6. Update collector tests to use `Batch`.

## Acceptance Criteria

- All existing tests pass unchanged (the `encoded_bytes`
  field is always empty at this stage).
- `cargo clippy --all-targets --all-features` is clean.
- `cargo fmt --check` is clean.
