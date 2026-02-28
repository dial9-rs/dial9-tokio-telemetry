//! Example: self-profile a workload and print a simple stack trace summary.
//!
//! Build with frame pointers:
//!   RUSTFLAGS="-C force-frame-pointers=yes" cargo run --release --example basic
//!
//! You may need:
//!   echo 1 | sudo tee /proc/sys/kernel/perf_event_paranoid

use dial9_perf_self_profile::{EventSource, PerfSampler, SamplerConfig, resolve_symbol};
use std::collections::HashMap;

fn main() {
    // --- Start the sampler ---
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

    // --- Do some work ---
    eprintln!("Running workload...");
    let result = do_work();
    eprintln!("Work result: {result}");

    // --- Stop and collect samples ---
    sampler.disable();

    let samples = sampler.drain_samples();
    eprintln!("\nCollected {} samples\n", samples.len());

    if samples.is_empty() {
        eprintln!("No samples collected. Make sure you're running with frame pointers.");
        eprintln!(
            "  RUSTFLAGS=\"-C force-frame-pointers=yes\" cargo run --release --example basic"
        );
        return;
    }

    // --- Print a few raw samples ---
    eprintln!("=== First 3 samples ===");
    for (i, sample) in samples.iter().take(3).enumerate() {
        eprintln!(
            "Sample {i}: ip={:#x}, tid={}, cpu={}, frames:",
            sample.ip, sample.tid, sample.cpu
        );
        for (j, &addr) in sample.callchain.iter().enumerate() {
            let sym = resolve_symbol(addr);
            let name = sym.name.as_deref().unwrap_or("???");
            eprintln!("  [{j:2}] {addr:#018x}  {name}+{:#x}", sym.offset);
        }
        eprintln!();
    }

    // --- Build a simple flat profile ---
    eprintln!("=== Flat profile (top functions by IP) ===");
    let mut counts: HashMap<String, u64> = HashMap::new();
    for sample in &samples {
        let sym = resolve_symbol(sample.ip);
        let name = sym.name.unwrap_or_else(|| format!("{:#x}", sample.ip));
        *counts.entry(name).or_default() += 1;
    }

    let mut sorted: Vec<_> = counts.into_iter().collect();
    sorted.sort_by(|a, b| b.1.cmp(&a.1));

    let total = samples.len() as f64;
    for (name, count) in sorted.iter().take(15) {
        let pct = (*count as f64 / total) * 100.0;
        eprintln!("  {count:5} ({pct:5.1}%)  {name}");
    }

    // --- Build a simple callee-based profile (which functions appear in stacks) ---
    eprintln!("\n=== Inclusive profile (top functions in any stack frame) ===");
    let mut inclusive: HashMap<String, u64> = HashMap::new();
    for sample in &samples {
        // Deduplicate within a single stack (recursive calls)
        let mut seen = std::collections::HashSet::new();
        for &addr in &sample.callchain {
            let sym = resolve_symbol(addr);
            let name = sym.name.unwrap_or_else(|| format!("{:#x}", addr));
            if seen.insert(name.clone()) {
                *inclusive.entry(name).or_default() += 1;
            }
        }
    }

    let mut sorted: Vec<_> = inclusive.into_iter().collect();
    sorted.sort_by(|a, b| b.1.cmp(&a.1));

    for (name, count) in sorted.iter().take(15) {
        let pct = (*count as f64 / total) * 100.0;
        eprintln!("  {count:5} ({pct:5.1}%)  {name}");
    }
}

// ---  Workload functions ---

#[inline(never)]
fn do_work() -> u64 {
    let mut total = 0u64;
    for i in 0..50 {
        total = total.wrapping_add(compute_primes(10_000));
        total = total.wrapping_add(compute_fibonacci(30 + (i % 10)));
    }
    total
}

#[inline(never)]
fn compute_primes(limit: u64) -> u64 {
    let mut count = 0u64;
    for n in 2..limit {
        if is_prime(n) {
            count += 1;
        }
    }
    count
}

#[inline(never)]
fn is_prime(n: u64) -> bool {
    if n < 2 {
        return false;
    }
    if n < 4 {
        return true;
    }
    if n % 2 == 0 || n % 3 == 0 {
        return false;
    }
    let mut i = 5;
    while i * i <= n {
        if n % i == 0 || n % (i + 2) == 0 {
            return false;
        }
        i += 6;
    }
    true
}

#[inline(never)]
fn compute_fibonacci(n: u32) -> u64 {
    if n <= 1 {
        return n as u64;
    }
    compute_fibonacci(n - 1) + compute_fibonacci(n - 2)
}
