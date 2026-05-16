# Agent Guidelines

- You MUST use Red/Green TDD.
- BEFORE COMMITING: `cargo nextest run --stress-duration 20s`. The package has no flaky tests. If you find a flaky test, you created it.
- **JS/HTML-only changes** (no `.rs` files touched, no trace format changes): you do NOT need to run the full Rust test suite or the stress test. Run the relevant JS tests under `dial9-viewer/ui/test_*.js` with `node <test>` and a quick `cargo build -p dial9-viewer` to confirm `rust-embed` picks up any new files. Skip `cargo nextest` / stress run.

## API Design

This is a published library with backwards compatibility requirements. Follow
these rules for all public APIs:

- **Use builders for all configuration.** Never use positional arguments for
  config that may grow. Use `#[bon::builder]` (v3) to derive builders.
- **All builder fields should be private** with setter methods, so we can add
  fields without breaking changes.
- **Prefer `impl Into<String>` over `&str`** in builder setters for ergonomics.
- **Non-required fields must have defaults.** New fields added later must be
  optional or defaulted to avoid breaking existing callers.
- **Mark config structs `#[non_exhaustive]`** if not using `#[bon::builder]`,
  so adding fields is not a breaking change.
- **Think about semver hazards:** adding a required parameter, removing a
  public type, or changing a trait signature are all breaking. When in doubt,
  keep it private or behind a builder.

## Trace Format Backwards Compatibility

The trace format uses a self-describing schema: each event type's schema is
written to the wire before any events of that type. Decoders use the schema on
the wire (not a compiled-in schema) to decode events.

**Rules:**

1. **Adding new fields is always safe** — even non-optional ones. The decoder
   reads whatever fields the schema declares. Old traces simply won't have the
   new field in their schema, so it won't appear in the decoded output.

2. **Removing non-optional fields is NOT safe.** Old traces that contain the
   field will still declare it in their on-wire schema, and the decoder will
   attempt to read it.

3. **We only care about the JS decoder reading old traces.** Users always have
   a current decoder (the viewer), but may load old trace files. When you add a
   new non-optional field, the JS viewer code that accesses it must handle the
   field being `undefined` (because old traces won't have it):

   ```js
   // Good — gracefully handles old traces missing the field
   const workerId = v.worker_id != null ? num(v.worker_id) : undefined;

   // Bad — will throw or produce NaN on old traces
   const workerId = num(v.worker_id);
   ```

4. **Rust decoder backwards compat is not a concern.** We don't need to worry
   about old Rust decoders reading new traces.

## Coding practices

**Never use `unwrap_or(0)`, `unwrap_or_default()`, or similar "fallback to
zero/sentinel" patterns.** These hide bugs by silently producing plausible but
wrong values. Always consider the actual error condition: propagate the error,
return `Option`, log and skip, or panic if the invariant is truly unrecoverable.

Avoid dropping an error without logging it. Use `tracing` for logging.
```
let _ = ...
```

**ALWAYS rate-limit logging that can fire from a loop or repeated error path.** Any `warn!`/`error!` reachable from a background task loop, retry loop, or per-request hot path MUST be wrapped in `rate_limited!`:
```rust
rate_limited!(Duration::from_secs(60), {
    tracing::warn!("...: {e}");
});
```
Unguarded logging in loops causes log spam that degrades observability and can itself become a performance problem. One-time paths (startup, shutdown, per-thread init) are exempt.

## Running tests

- Always run `cargo nextest run` to run tests

## Ownership

- You own all code you are working on. There is no such thing as a "pre-existing failure" or something that is "not your problem." If you see a warning or failure, fix it.

## Pre-commit checks

- You MUST run `cargo fmt --check` and `cargo clippy --all-targets --all-features` before every commit. Both must be clean.

## Demo Trace

If you modify the trace format (event structure, encoding, parser, etc.), you MUST regenerate the demo trace:

```bash
./scripts/regenerate_demo_trace.sh
```

Or via Docker (no host Rust/AWS/Java needed — DDB Local runs as a sidecar):

```bash
./scripts/regenerate_demo_trace_docker.sh
```

Or manually:

```bash
rm -f dial9-viewer/ui/demo-trace.bin
cargo build --release -p metrics-service
AWS_PROFILE=your-profile cargo run --release -p metrics-service --bin metrics-service -- --trace-path sched-trace.bin --demo
cp sched-trace.*.bin dial9-viewer/ui/demo-trace.bin
git add dial9-viewer/ui/demo-trace.bin
git commit -m "Regenerate demo trace after format changes"
```

The demo trace is used for:
- Live demos on the hosted viewer
- Documentation screenshots
- Testing the viewer with real data

Failing to update it will cause the viewer to fail when loading the demo.

## Meta

- Never declare done after pushing or opening a PR until CI is green. Check CI status and fix any failures before moving on.

## Ways of working
- After finishing your work use showboat to demonstrate what you have done. include key code, tests, and what was changed.
- Use a progress doc to keep track of what you are doing. progress docs are just for you. when we are ready for PR, we unstage the progress docs
