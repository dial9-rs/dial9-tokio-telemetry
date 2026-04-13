# TelemetryCore API Refactor

## Goal
Decouple telemetry session creation from tokio runtime building. New entry point:
`TelemetryCore::builder().writer(w).build()` → `TelemetryGuard`, then
`guard.trace_runtime("name", builder)` → `Runtime`.

## Design Decisions
- `TelemetryCore` is the builder namespace, `.build()` produces `TelemetryGuard`
- `#[bon::builder]` for `TelemetryCore::build()`, writer is required param
- `TelemetryGuard::trace_runtime()` installs hooks + builds runtime, returns `Runtime`
- `TracedRuntime::builder()` kept as convenience, reimplemented on top of TelemetryCore
- `build_with_reuse()` delegates to `trace_runtime()`

## Progress
- [ ] Extract TelemetryCore builder + build()
- [ ] Implement TelemetryGuard::trace_runtime()
- [ ] Reimplement TracedRuntime on top of TelemetryCore
- [ ] Update thread_per_core example
- [ ] Update multi_runtime example + README
- [ ] Tests, fmt, clippy, stress test
