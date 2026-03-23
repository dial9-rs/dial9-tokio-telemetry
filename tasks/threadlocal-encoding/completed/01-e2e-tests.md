# Task 01: End-to-End Tests for Thread-Local Encoding

Agent: implementer
Design: docs/design/threadlocal-encoding.md

## Objective

- Create end-to-end tests that validate the thread-local
encoding pipeline: events encoded on worker threads via
per-thread `Encoder<Vec<u8>>` are transcoded by the flush
thread and produce identical trace output to the current
direct-encode path.

- Create proptests the validate the behavior across the entire input space

## Test File

`dial9-trace-format/tests/threadlocal_encoding.rs`

## Test Cases

### Encoder reset

```
test_reset_to_returns_decodable_bytes
```
Given an `Encoder<Vec<u8>>` that has encoded several events,
When `reset_to` is called with a fresh `Vec<u8>`,
Then the returned `Vec<u8>` decodes to the original events,
And the encoder produces a valid header on the new writer,
And internal allocations (string pool HashMap capacity,
schema_ids capacity) do not shrink.

```
test_reset_convenience_returns_decodable_bytes
```
Given an `Encoder<Vec<u8>>` that has encoded events,
When `reset()` is called,
Then the returned bytes decode to the expected events,
And the encoder is ready for a new encoding session.

### Transcoding round-trip

```
test_transcode_round_trip_single_batch
```
Given a batch of events encoded by one `Encoder<Vec<u8>>`,
When the bytes are transcoded through a `Transcoder` into a
second `Encoder<Vec<u8>>`,
Then decoding the second encoder's output yields identical
event names, field values, and absolute timestamps.

```
test_transcode_string_pool_remapping
```
Given two batches from different encoders that intern the
same strings with different local pool IDs,
When both are transcoded into a single target encoder,
Then the target encoder assigns a single global pool ID per
unique string (no duplicates in the output pool).

```
test_transcode_timestamp_rebasing
```
Given two batches from encoders with different timestamp
bases,
When both are transcoded into a target encoder,
Then the decoded output contains correct absolute timestamps
for all events (no drift or precision loss).

```
test_transcode_schema_deduplication
```
Given two batches that each register the same schema,
When both are transcoded into a single target encoder,
Then the target encoder contains only one copy of the schema,
And events from both batches reference the same wire type ID.

```
test_transcode_empty_batch
```
Given an encoded byte stream that contains only a header
(no events),
When transcoded through a `Transcoder`,
Then no events are written to the target encoder,
And no error is returned.

### Fallible iteration

```
test_try_for_each_event_propagates_error
```
Given a valid encoded byte stream,
When `try_for_each_event` is called with a callback that
returns `Err` on the second event,
Then `try_for_each_event` returns that error,
And only the first event was processed.

```
test_try_for_each_event_success
```
Given a valid encoded byte stream,
When `try_for_each_event` is called with a callback that
always returns `Ok(())`,
Then all events are processed and `Ok(())` is returned.

## Notes

- All tests use `Encoder<Vec<u8>>` and `Decoder` directly —
  no file I/O or Tokio runtime needed.
- Use the existing `PollStartEvent` / `PollEndEvent` derive
  types from `dial9-tokio-telemetry/src/telemetry/format.rs`
  or define minimal test-local `#[derive(TraceEvent)]`
  structs if the telemetry crate types are not accessible
  from `dial9-trace-format` tests.
- Timestamps should use known absolute values so assertions
  can verify exact round-trip fidelity.

## Acceptance Criteria

- All tests compile.
- All tests fail with clear, descriptive assertion messages
  indicating what functionality is missing (e.g.,
  `Encoder has no method reset_to`,
  `Transcoder type does not exist`).
- No test panics with a cryptic message or stack trace.
