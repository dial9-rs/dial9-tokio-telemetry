# Task 07: MVP Benchmark

Agent: implementer
Design: docs/design/threadlocal-encoding.md
Depends on: Task 02, Task 03

## Objective

Create a benchmark that measures the end-to-end cost of
thread-local encode → transcode → central encode vs. the
current direct-encode path, establishing a performance
baseline.

## Target Tests

No Task 01 tests. This is a benchmark task.

## Implementation

File: `dial9-tokio-telemetry/benches/threadlocal_encode.rs`

Use the existing `writer_encode.rs` bench as a starting
point. Create two benchmark functions:

### `bench_direct_encode`

Baseline: encode a batch of events directly through a
single `Encoder<Vec<u8>>` (the current path). Use
representative event types (`PollStartEvent`,
`PollEndEvent`, `WorkerParkEvent`, `WorkerUnparkEvent`).

### `bench_threadlocal_transcode`

New path: encode the same batch through a thread-local
`Encoder<Vec<u8>>`, call `reset()` to get the bytes, then
transcode through a `Transcoder` into a central
`Encoder<Vec<u8>>`.

Both benchmarks should:
- Use the same event sequence for fair comparison.
- Report throughput in events/sec.
- Use `criterion` (matching the existing bench setup).

Add the bench target to `dial9-tokio-telemetry/Cargo.toml`
if not already present.

## Acceptance Criteria

- `cargo bench --bench threadlocal_encode` runs and
  produces comparison numbers.
- No existing benchmarks are broken.
- `cargo clippy --all-targets --all-features` is clean.
