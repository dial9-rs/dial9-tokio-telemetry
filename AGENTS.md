# Agent Guidelines

- You MUST use Red/Green TDD.
- BEFORE COMMITING: `cargo nextest run --stress-duration 20`. The package has no flaky tests. If you find a flaky test, you created it.

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

## Coding practices
Avoid dropping an error without logging it. Use `tracing` for logging.
```
let _ = ...
```

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

Or manually:

```bash
cd dial9-tokio-telemetry
rm -f trace_viewer/demo-trace.bin
cargo build --release -p metrics-service
AWS_PROFILE=your-profile cargo run --release -p metrics-service --bin metrics-service -- --trace-path sched-trace.bin --demo
cp sched-trace.*.bin trace_viewer/demo-trace.bin
git add trace_viewer/demo-trace.bin
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
