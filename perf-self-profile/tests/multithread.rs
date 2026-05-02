#![cfg(target_os = "linux")]

use dial9_perf_self_profile::{EventSource, PerfSampler, SamplerConfig, SamplingMode};
use std::collections::HashSet;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::thread;

#[inline(never)]
fn burn_cpu(stop: &AtomicBool) {
    let mut sum = 0u64;
    let mut i = 0u64;
    while !stop.load(Ordering::Relaxed) {
        sum = sum.wrapping_add(i);
        std::hint::black_box(sum);
        i += 1;
    }
}

#[test]
fn profiles_spawned_threads() {
    let mut sampler = PerfSampler::start(
        SamplerConfig::default()
            .event_source(EventSource::SwCpuClock)
            .sampling(SamplingMode::FrequencyHz(999)),
    )
    .expect("failed to start sampler");

    let stop = Arc::new(AtomicBool::new(false));

    let handles: Vec<_> = (0..4)
        .map(|_| {
            let stop = stop.clone();
            thread::spawn(move || burn_cpu(&stop))
        })
        .collect();

    // Let threads run long enough to collect samples
    thread::sleep(std::time::Duration::from_millis(200));
    stop.store(true, Ordering::Relaxed);

    for h in handles {
        h.join().unwrap();
    }

    sampler.disable();
    let samples = sampler.drain_samples();

    let tids: HashSet<u32> = samples.iter().map(|s| s.tid).collect();
    assert!(!samples.is_empty(), "expected at least some samples, got 0");
    assert!(
        tids.len() > 1,
        "expected samples from multiple threads, but only saw tids: {:?} ({} samples)",
        tids,
        samples.len()
    );

    // Perf events carry a CPU id via PERF_SAMPLE_CPU. Expect every sample to
    // have Some(cpu) — the perf path never reports an unknown cpu.
    let without_cpu: usize = samples.iter().filter(|s| s.cpu.is_none()).count();
    assert_eq!(
        without_cpu,
        0,
        "expected every perf sample to carry a cpu id, got {without_cpu}/{} without",
        samples.len(),
    );
}
