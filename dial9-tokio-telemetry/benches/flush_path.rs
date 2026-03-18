//! Benchmark for the flush path: N producer threads pushing batches into
//! CentralCollector while one consumer thread drains and writes events.
//!
//! This runs outside the Tokio runtime to isolate flush-path performance.
//!
//! Usage:
//!   cargo bench --bench flush_path

use criterion::{BenchmarkId, Criterion, criterion_group, criterion_main};
use dial9_tokio_telemetry::telemetry::events::RawEvent;
use dial9_tokio_telemetry::telemetry::{NullWriter, TaskId, TraceWriter, WorkerId};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Barrier};

/// Build a realistic batch simulating a worker thread's activity.
///
/// A typical worker cycle is: unpark → (poll_start, poll_end) × N → park.
/// We simulate ~170 polls per batch (340 events) plus park/unpark and a few
/// spawns and wakes, totalling ~350 events. The batch is repeated ~3× to fill
/// close to BUFFER_CAPACITY (1024).
fn make_batch(worker: usize) -> Vec<RawEvent> {
    let wid = WorkerId::from(worker);
    let task = TaskId::from_u32(1);
    let loc = std::panic::Location::caller();
    let mut events = Vec::with_capacity(1024);

    // ~3 cycles of unpark → polls → park to fill the buffer
    for _ in 0..3 {
        events.push(RawEvent::WorkerUnpark {
            timestamp_nanos: 100,
            worker_id: wid,
            worker_local_queue_depth: 5,
            cpu_time_nanos: 500_000,
            sched_wait_delta_nanos: 1_000,
        });

        // ~170 poll pairs per cycle
        for i in 0..170 {
            events.push(RawEvent::PollStart {
                timestamp_nanos: 200 + i * 10,
                worker_id: wid,
                worker_local_queue_depth: 3,
                task_id: task,
                location: loc,
            });
            events.push(RawEvent::PollEnd {
                timestamp_nanos: 205 + i * 10,
                worker_id: wid,
            });
        }

        // A few task spawns and wake events per cycle
        for _ in 0..3 {
            events.push(RawEvent::TaskSpawn {
                timestamp_nanos: 2000,
                task_id: task,
                location: loc,
            });
        }
        for _ in 0..5 {
            events.push(RawEvent::WakeEvent {
                timestamp_nanos: 2500,
                waker_task_id: task,
                woken_task_id: task,
                target_worker: worker as u8,
            });
        }

        events.push(RawEvent::WorkerPark {
            timestamp_nanos: 3000,
            worker_id: wid,
            worker_local_queue_depth: 0,
            cpu_time_nanos: 600_000,
        });
    }

    events
}

/// Simulate the full flush path: N producer threads each push `batches_per_producer`
/// batches into a shared CentralCollector. One consumer thread continuously
/// drains and writes through a NullWriter.
fn run_flush_path(num_producers: usize, batches_per_producer: usize) -> u64 {
    use dial9_tokio_telemetry::telemetry::collector::CentralCollector;

    let collector = Arc::new(CentralCollector::new());
    let stop = Arc::new(AtomicBool::new(false));
    let barrier = Arc::new(Barrier::new(num_producers + 1));

    let producers: Vec<_> = (0..num_producers)
        .map(|i| {
            let collector = collector.clone();
            let barrier = barrier.clone();
            std::thread::spawn(move || {
                barrier.wait();
                for _ in 0..batches_per_producer {
                    collector.accept_flush(make_batch(i));
                }
            })
        })
        .collect();

    let consumer = {
        let collector = collector.clone();
        let stop = stop.clone();
        let barrier = barrier.clone();
        std::thread::spawn(move || {
            let mut writer: Box<dyn TraceWriter> = Box::new(NullWriter);
            let mut total: u64 = 0;
            barrier.wait();
            loop {
                let batches = collector.drain();
                if batches.is_empty() {
                    if stop.load(Ordering::Acquire) {
                        for batch in collector.drain() {
                            total += batch.len() as u64;
                            for event in batch {
                                let _ = writer.write_event(&event);
                            }
                        }
                        break;
                    }
                    std::thread::yield_now();
                    continue;
                }
                for batch in batches {
                    total += batch.len() as u64;
                    for event in batch {
                        let _ = writer.write_event(&event);
                    }
                }
            }
            total
        })
    };

    for p in producers {
        p.join().unwrap();
    }
    stop.store(true, Ordering::Release);
    consumer.join().unwrap()
}

fn bench_flush_path(c: &mut Criterion) {
    let mut group = c.benchmark_group("flush_path");
    let batches_per_producer = 200;

    for num_producers in [1, 4, 8, 16] {
        group.bench_with_input(
            BenchmarkId::new("producers", num_producers),
            &num_producers,
            |b, &np| {
                b.iter(|| run_flush_path(np, batches_per_producer));
            },
        );
    }
    group.finish();
}

/// Benchmark just the accept_flush (producer side) in isolation — no consumer.
fn bench_accept_flush(c: &mut Criterion) {
    use dial9_tokio_telemetry::telemetry::collector::CentralCollector;

    let mut group = c.benchmark_group("accept_flush");

    for num_threads in [1, 4, 8] {
        group.bench_with_input(
            BenchmarkId::new("threads", num_threads),
            &num_threads,
            |b, &nt| {
                b.iter(|| {
                    let collector = Arc::new(CentralCollector::new());
                    let barrier = Arc::new(Barrier::new(nt + 1));
                    let threads: Vec<_> = (0..nt)
                        .map(|i| {
                            let collector = collector.clone();
                            let barrier = barrier.clone();
                            std::thread::spawn(move || {
                                barrier.wait();
                                for _ in 0..100 {
                                    collector.accept_flush(make_batch(i));
                                }
                            })
                        })
                        .collect();
                    barrier.wait();
                    for t in threads {
                        t.join().unwrap();
                    }
                });
            },
        );
    }
    group.finish();
}

criterion_group!(benches, bench_flush_path, bench_accept_flush);
criterion_main!(benches);
