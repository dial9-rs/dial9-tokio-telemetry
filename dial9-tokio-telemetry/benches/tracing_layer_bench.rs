//! Micro-benchmark for the tracing layer's per-span overhead.
//!
//! Uses a current_thread runtime so block_on runs on the worker thread
//! (which has a TelemetryHandle). This measures the actual encoding cost.
//!
//! Two groups:
//! - `tracing_only`: spans with registry subscriber (no dial9 encoding)
//! - `with_dial9`: spans with Dial9TokioLayer (full encoding path)
//!
//! The difference between the two is the dial9 encoding overhead.
//!
//! Usage:
//!   cargo bench --bench tracing_layer_bench --features tracing-layer

use criterion::{BenchmarkId, Criterion, criterion_group, criterion_main};
use dial9_tokio_telemetry::telemetry::{NullWriter, TracedRuntime};
use dial9_tokio_telemetry::tracing_layer::Dial9TokioLayer;
use tracing_subscriber::prelude::*;

fn bench_tracing_only(c: &mut Criterion) {
    let mut group = c.benchmark_group("tracing_only");

    // current_thread runtime: block_on IS the worker thread
    let mut builder = tokio::runtime::Builder::new_current_thread();
    builder.enable_all();
    let (runtime, _guard) = TracedRuntime::builder()
        .build_and_start(builder, NullWriter)
        .unwrap();

    // Registry only, no dial9 layer. Use set_default (thread-local) since
    // set_global_default can only be called once.
    let subscriber = tracing_subscriber::registry();
    let _sub_guard = tracing::subscriber::set_default(subscriber);

    group.bench_function("baseline", |b| {
        b.iter(|| {
            runtime.block_on(async { std::hint::black_box(42) });
        });
    });

    for depth in [1, 3, 5] {
        group.bench_with_input(BenchmarkId::new("depth", depth), &depth, |b, &depth| {
            b.iter(|| {
                runtime.block_on(async { nested_spans(depth) });
            });
        });
    }

    group.bench_function("with_fields", |b| {
        b.iter(|| {
            runtime.block_on(async {
                let span = tracing::info_span!("fielded", user_id = 42, method = "GET", path = "/api/v1/users");
                let _enter = span.enter();
            });
        });
    });

    group.finish();
}

fn bench_with_dial9(c: &mut Criterion) {
    let mut group = c.benchmark_group("with_dial9");

    let mut builder = tokio::runtime::Builder::new_current_thread();
    builder.enable_all();
    let (runtime, _guard) = TracedRuntime::builder()
        .build_and_start(builder, NullWriter)
        .unwrap();

    let subscriber = tracing_subscriber::registry().with(Dial9TokioLayer::new());
    let _sub_guard = tracing::subscriber::set_default(subscriber);

    group.bench_function("baseline", |b| {
        b.iter(|| {
            runtime.block_on(async { std::hint::black_box(42) });
        });
    });

    for depth in [1, 3, 5] {
        group.bench_with_input(BenchmarkId::new("depth", depth), &depth, |b, &depth| {
            b.iter(|| {
                runtime.block_on(async { nested_spans(depth) });
            });
        });
    }

    group.bench_function("with_fields", |b| {
        b.iter(|| {
            runtime.block_on(async {
                let span = tracing::info_span!("fielded", user_id = 42, method = "GET", path = "/api/v1/users");
                let _enter = span.enter();
            });
        });
    });

    group.finish();
}

fn nested_spans(depth: usize) {
    if depth == 0 {
        return;
    }
    let span = tracing::info_span!("nested", level = depth);
    let _enter = span.enter();
    nested_spans(depth - 1);
}

criterion_group!(benches, bench_tracing_only, bench_with_dial9);
criterion_main!(benches);
