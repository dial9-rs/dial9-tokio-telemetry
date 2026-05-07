//! IAI callgrind bench for the tracing layer's single-thread per-span overhead.
//!
//! Two groups, each with baseline / depth 1·3·5 / with_fields:
//! - `tracing_only`: spans with registry subscriber (no dial9 encoding)
//! - `with_dial9`: spans with `Dial9TokioLayer` (full encoding path)
//!
//! Multi-thread contention scaling is in `tracing_layer_bench.rs`.
//!
//! Usage:
//!   cargo bench --bench tracing_layer_iai --features tracing-layer
//!
//! Gated on `--cfg iai_enabled` so plain `cargo test --all-targets`
//! compiles to a no-op stub `fn main()` instead of spawning
//! `iai-callgrind-runner`. CI iai jobs set RUSTFLAGS to enable.

#![cfg_attr(not(iai_enabled), allow(unused))]

#[cfg(not(iai_enabled))]
fn main() {}

use dial9_tokio_telemetry::telemetry::{NullWriter, TelemetryGuard, TracedRuntime};
use dial9_tokio_telemetry::tracing_layer::Dial9TokioLayer;
use iai_callgrind::{library_benchmark, library_benchmark_group, main};
use std::hint::black_box;
use tokio::runtime::Runtime;
use tracing::subscriber::DefaultGuard;
use tracing_subscriber::prelude::*;

struct Harness {
    runtime: Runtime,
    _telemetry_guard: TelemetryGuard,
    _sub_guard: DefaultGuard,
}

fn setup_tracing_only() -> Harness {
    let mut builder = tokio::runtime::Builder::new_current_thread();
    builder.enable_all();
    let (runtime, _telemetry_guard) = TracedRuntime::builder()
        .build_and_start(builder, NullWriter)
        .unwrap();
    let _sub_guard = tracing::subscriber::set_default(tracing_subscriber::registry());
    Harness {
        runtime,
        _telemetry_guard,
        _sub_guard,
    }
}

fn setup_with_dial9() -> Harness {
    let mut builder = tokio::runtime::Builder::new_current_thread();
    builder.enable_all();
    let (runtime, _telemetry_guard) = TracedRuntime::builder()
        .build_and_start(builder, NullWriter)
        .unwrap();
    let _sub_guard = tracing::subscriber::set_default(
        tracing_subscriber::registry().with(Dial9TokioLayer::new()),
    );
    Harness {
        runtime,
        _telemetry_guard,
        _sub_guard,
    }
}

fn nested_spans(depth: usize) {
    if depth == 0 {
        return;
    }
    let span = tracing::info_span!("nested", level = depth);
    let _enter = span.enter();
    nested_spans(depth - 1);
}

const ITERATIONS_PER_BENCH: usize = 10000;

#[inline(never)]
fn run_baseline(h: &Harness) -> i32 {
    let mut sum = 0i32;
    for _ in 0..ITERATIONS_PER_BENCH {
        sum = sum.wrapping_add(h.runtime.block_on(async { black_box(42) }));
    }
    sum
}

#[inline(never)]
fn run_depth(h: &Harness, depth: usize) {
    for _ in 0..ITERATIONS_PER_BENCH {
        h.runtime.block_on(async {
            nested_spans(black_box(depth));
        });
    }
}

#[inline(never)]
fn run_fields(h: &Harness) {
    for _ in 0..ITERATIONS_PER_BENCH {
        h.runtime.block_on(async {
            let span = tracing::info_span!(
                "fielded",
                user_id = 42,
                method = "GET",
                path = "/api/v1/users"
            );
            let _enter = span.enter();
        });
    }
}

#[library_benchmark]
#[bench::baseline(setup_tracing_only())]
fn tracing_only_baseline(h: Harness) -> i32 {
    black_box(run_baseline(&h))
}

fn setup_tracing_only_with_depth(depth: usize) -> (Harness, usize) {
    (setup_tracing_only(), depth)
}

#[library_benchmark]
#[bench::depth_1(setup_tracing_only_with_depth(1))]
#[bench::depth_3(setup_tracing_only_with_depth(3))]
#[bench::depth_5(setup_tracing_only_with_depth(5))]
fn tracing_only_depth((h, depth): (Harness, usize)) {
    run_depth(&h, depth)
}

#[library_benchmark]
#[bench::with_fields(setup_tracing_only())]
fn tracing_only_with_fields(h: Harness) {
    run_fields(&h)
}

#[library_benchmark]
#[bench::baseline(setup_with_dial9())]
fn with_dial9_baseline(h: Harness) -> i32 {
    black_box(run_baseline(&h))
}

fn setup_with_dial9_with_depth(depth: usize) -> (Harness, usize) {
    (setup_with_dial9(), depth)
}

#[library_benchmark]
#[bench::depth_1(setup_with_dial9_with_depth(1))]
#[bench::depth_3(setup_with_dial9_with_depth(3))]
#[bench::depth_5(setup_with_dial9_with_depth(5))]
fn with_dial9_depth((h, depth): (Harness, usize)) {
    run_depth(&h, depth)
}

#[library_benchmark]
#[bench::with_fields(setup_with_dial9())]
fn with_dial9_with_fields(h: Harness) {
    run_fields(&h)
}

library_benchmark_group!(
    name = tracing_only_group;
    benchmarks = tracing_only_baseline, tracing_only_depth, tracing_only_with_fields
);

library_benchmark_group!(
    name = with_dial9_group;
    benchmarks = with_dial9_baseline, with_dial9_depth, with_dial9_with_fields
);

#[cfg(iai_enabled)]
main!(
    library_benchmark_groups = tracing_only_group,
    with_dial9_group
);
