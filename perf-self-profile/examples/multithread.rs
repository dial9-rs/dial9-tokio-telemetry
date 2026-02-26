//! Example: profile all threads spawned by the process.
//!
//! Build with frame pointers:
//!   RUSTFLAGS="-C force-frame-pointers=yes" cargo run --release --example multithread

use perf_self_profile::{EventSource, PerfSampler, SamplerConfig};
use std::collections::HashMap;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::thread;

fn main() {
    let mut sampler = match PerfSampler::start(SamplerConfig {
        frequency_hz: 999,
        event_source: EventSource::SwCpuClock,
        include_kernel: false,
    }) {
        Ok(s) => s,
        Err(e) => {
            eprintln!("Failed to start sampler: {e}");
            eprintln!("Try: echo 1 | sudo tee /proc/sys/kernel/perf_event_paranoid");
            std::process::exit(1);
        }
    };

    let stop = Arc::new(AtomicBool::new(false));

    let handles: Vec<_> = (0..4)
        .map(|i| {
            let stop = stop.clone();
            thread::Builder::new()
                .name(format!("worker-{i}"))
                .spawn(move || cpu_work(&stop))
                .unwrap()
        })
        .collect();

    // Let threads run
    thread::sleep(std::time::Duration::from_secs(1));
    stop.store(true, Ordering::Relaxed);

    for h in handles {
        h.join().unwrap();
    }

    sampler.disable();
    let samples = sampler.drain_samples();
    eprintln!("Collected {} samples", samples.len());

    // Show samples per thread
    let mut by_tid: HashMap<u32, usize> = HashMap::new();
    for s in &samples {
        *by_tid.entry(s.tid).or_default() += 1;
    }
    let mut tids: Vec<_> = by_tid.into_iter().collect();
    tids.sort_by(|a, b| b.1.cmp(&a.1));
    for (tid, count) in &tids {
        eprintln!("  tid={tid}: {count} samples");
    }
}

#[inline(never)]
fn cpu_work(stop: &AtomicBool) -> u64 {
    let mut sum = 0u64;
    let mut i = 0u64;
    while !stop.load(Ordering::Relaxed) {
        sum = sum.wrapping_add(i);
        std::hint::black_box(sum);
        i += 1;
    }
    sum
}
