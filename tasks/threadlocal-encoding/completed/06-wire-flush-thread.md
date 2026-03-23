# Task 06: Wire Flush Thread to Transcode Encoded Batches

Agent: implementer
Design: docs/design/threadlocal-encoding.md
Depends on: Task 03, Task 04, Task 05

## Objective

Update `flush_once` and `EventWriter` to process both
halves of a `Batch`: raw events through the existing path,
and encoded bytes through the `Transcoder`.

## Target Tests

Validated by existing end-to-end and integration tests
continuing to pass. Since `should_encode` returns `false`
for all variants (Task 05), `encoded_bytes` will only
contain a header — the transcoder path is exercised but
produces no events.

## Implementation

### TraceWriter trait

File: `dial9-tokio-telemetry/src/telemetry/writer.rs`

Add a default method to `TraceWriter`:

```rust
fn write_encoded_batch(
    &mut self,
    _bytes: &[u8],
    _transcoder: &mut Transcoder,
) -> std::io::Result<()> {
    Ok(()) // default no-op for NullWriter, test writers
}
```

Implement on `RotatingWriter`: transcode `bytes` through
the `Transcoder` into the `RotatingWriter`'s internal
`Encoder`, then call `maybe_rotate()`.

This follows Option A from the design: the `TraceWriter`
trait gains one additive method with a default impl, so
`NullWriter` and test helpers are unaffected.

### EventWriter

File: `dial9-tokio-telemetry/src/telemetry/recorder/event_writer.rs`

1. Add `transcoder: Transcoder` field to `EventWriter`.
   Initialize in `EventWriter::new` with
   `Transcoder::new()`.

2. Add method:
   ```rust
   pub(crate) fn write_transcoded_batch(
       &mut self,
       encoded_bytes: &[u8],
   ) -> std::io::Result<()> {
       self.writer.write_encoded_batch(
           encoded_bytes,
           &mut self.transcoder,
       )
   }
   ```

### flush_once

File: `dial9-tokio-telemetry/src/telemetry/recorder/mod.rs`

Update the drain loop in `flush_once`. Note: after Task 05,
`encoded_bytes` may contain only a file header (no events)
when `should_encode` returns `false` for all variants. The
`Transcoder` handles header-only batches gracefully (no
events to transcode), so no special-casing is needed beyond
the existing `!batch.encoded_bytes.is_empty()` check.

```rust
while let Some(batch) = shared.collector.next() {
    for raw in batch.raw_events {
        if let Err(e) = event_writer.write_raw_event(raw) {
            // existing error handling
        }
    }
    if !batch.encoded_bytes.is_empty() {
        if let Err(e) = event_writer
            .write_transcoded_batch(&batch.encoded_bytes)
        {
            tracing::warn!(
                "failed to transcode batch: {e}"
            );
            shared.enabled.store(false, Ordering::Relaxed);
            // return early with stats
        }
    }
}
```

## Acceptance Criteria

- All existing tests pass.
- `cargo clippy --all-targets --all-features` is clean.
- `cargo fmt --check` is clean.
