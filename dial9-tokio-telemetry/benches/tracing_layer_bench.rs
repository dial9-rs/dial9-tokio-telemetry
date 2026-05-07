//! Multi-thread bench for the tracing layer.
//!
//! Measures span emission throughput as N threads share one
//! `Dial9TokioLayer` (and therefore one schemas Mutex). Each thread
//! emits 100 spans after a barrier sync; the iteration is timed end to
//! end across N ∈ {1, 2, 4, 8, 16, 32}.
//!
//! Usage:
//!   cargo bench --bench tracing_layer_bench --features tracing-layer

use criterion::{BenchmarkId, Criterion, criterion_group, criterion_main};
use dial9_tokio_telemetry::tracing_layer::Dial9TokioLayer;
use std::sync::{Arc, Barrier};
use tracing::Dispatch;
use tracing_subscriber::prelude::*;

fn bench_multi_thread(c: &mut Criterion) {
    let mut group = c.benchmark_group("multi_thread");
    group.measurement_time(std::time::Duration::from_secs(8));

    // One shared subscriber (one Dial9TokioLayer, one schemas Mutex).
    // All threads dispatch through this single layer, contending on the mutex.
    let dispatch = Dispatch::new(tracing_subscriber::registry().with(Dial9TokioLayer::new()));

    let spans_per_thread = 100;

    for threads in [1, 2, 4, 8, 16, 32] {
        group.bench_with_input(
            BenchmarkId::new("schema_contention", threads),
            &threads,
            |b, &n| {
                b.iter(|| {
                    let barrier = Arc::new(Barrier::new(n));
                    std::thread::scope(|s| {
                        for _ in 0..n {
                            let d = dispatch.clone();
                            let bar = barrier.clone();
                            s.spawn(move || {
                                let _g = tracing::dispatcher::set_default(&d);
                                bar.wait();
                                for _ in 0..spans_per_thread {
                                    let span = tracing::info_span!("contended", key = "value");
                                    let _enter = span.enter();
                                }
                            });
                        }
                    });
                });
            },
        );
    }

    group.finish();
}

criterion_group!(benches, bench_multi_thread);
criterion_main!(benches);
