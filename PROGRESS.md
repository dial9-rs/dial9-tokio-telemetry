# Eager worker_ids population

## What changed
Pre-populate `RuntimeContext::worker_ids` in `attach_runtime()` immediately after `metrics_and_base` is set, instead of lazily on each worker thread's first poll/park event.

## Why
When a runtime is attached, we already know `num_workers` and `base` from `RuntimeMetrics`. Eagerly populating the map means `metadata_entry()` returns complete runtime→worker mappings from the very first flush cycle, rather than converging over time as workers come online.

## Change
One block added in `attach_runtime()` (recorder/mod.rs):
```rust
{
    let mut ids = ctx.worker_ids.write().unwrap();
    for i in 0..num_workers {
        ids.insert(i as usize, base + i);
    }
}
```

`register_worker_if_needed` remains idempotent — guarded by a thread-local flag, and the insert is a no-op overwrite with the same value.

## Verification
- `cargo fmt --check` ✅
- `cargo clippy --all-targets --all-features` ✅
- `cargo nextest run` — 460/461 pass (1 pre-existing CPU-load-sensitive flaky test in dial9-perf-self-profile)
- `cargo nextest run --stress-duration 20s` — 2 iterations, 460/460 pass
