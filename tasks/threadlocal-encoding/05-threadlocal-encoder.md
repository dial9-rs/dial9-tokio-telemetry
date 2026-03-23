# Task 05: Thread-Local Encoding in Buffer

Agent: implementer
Design: docs/design/threadlocal-encoding.md
Depends on: Task 02, Task 04

## Objective

Add an `Encoder<Vec<u8>>` to `ThreadLocalBuffer` alongside
the existing `Vec<RawEvent>`, with `should_encode` routing
and `encode_event`. On flush, produce a `Batch` containing
both `raw_events` and `encoded_bytes`.

## Target Tests

No Task 01 tests target this directly (the E2E tests are
in `dial9-trace-format`). Validated by existing integration
tests in `dial9-tokio-telemetry` continuing to pass.

## Implementation

File: `dial9-tokio-telemetry/src/telemetry/buffer.rs`

1. Add `Encoder<Vec<u8>>` and `event_count: usize` fields
   to `ThreadLocalBuffer`. Add a `location_cache:
   HashMap<&'static Location<'static>, String>` for
   caching formatted location strings.

2. Add `should_encode(event: &RawEvent) -> bool` that
   initially returns `false` for all variants. This is the
   migration switch — as event types are migrated, their
   match arms flip to `true`.

3. Add `encode_event(&mut self, event: &RawEvent)` that
   mirrors the encoding logic in
   `RotatingWriter::write_resolved_no_rotate`
   (`writer.rs:308–419`) but writes to `self.encoder`.
   Use `self.location_cache` to cache
   `Location::to_string()` results, then call
   `self.encoder.intern_string(&cached_string)` each time.

4. Update `record_event` to route via `should_encode`:
   ```rust
   fn record_event(&mut self, event: RawEvent) {
       if Self::should_encode(&event) {
           self.encode_event(&event);
       } else {
           self.events.push(event);
       }
       self.event_count += 1;
   }
   ```

5. Update `should_flush` to use `self.event_count >=
   BUFFER_CAPACITY` instead of `self.events.len()`.

6. Update `flush` to return `Batch`:
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
       Batch { raw_events, encoded_bytes }
   }
   ```
   Define `ESTIMATED_BATCH_SIZE` as a reasonable constant
   (e.g., 16 * 1024).

7. Update the `Drop` impl to flush both paths.

8. Update the module-level `record_event` and
   `drain_to_collector` functions to pass `Batch` to the
   collector.

9. Update buffer tests.

## Assumption

`should_encode` returns `false` for all variants in this
task. No events are actually routed to the encoder yet.
This means `encoded_bytes` in the `Batch` will contain
only the file header (from `Encoder::new()`). The flush
thread (Task 06) must handle this gracefully.

## Acceptance Criteria

- All existing tests pass (behavior is unchanged since no
  events are routed to the encoder).
- `cargo clippy --all-targets --all-features` is clean.
- `cargo fmt --check` is clean.
