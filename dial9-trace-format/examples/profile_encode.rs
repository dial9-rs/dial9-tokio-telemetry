//! Profile the encoder using dial9-perf-self-profile.
//! Run: cargo run --example profile_encode --release

use dial9_perf_self_profile::{EventSource, PerfSampler, SamplerConfig, resolve_symbol};
use dial9_trace_format::encoder::Encoder;
use dial9_trace_format::{StackFrames, TraceEvent};
use std::collections::HashMap;

#[derive(TraceEvent)]
struct PollStart { timestamp_ns: u64, worker_id: u64, local_queue_depth: u64, task_id: u64, spawn_loc_id: u64 }
#[derive(TraceEvent)]
struct PollEnd { timestamp_ns: u64, worker_id: u64 }
#[derive(TraceEvent)]
struct WorkerPark { timestamp_ns: u64, worker_id: u64, local_queue_depth: u64, cpu_time_ns: u64 }
#[derive(TraceEvent)]
struct WakeEvent { timestamp_ns: u64, waker_task_id: u64, woken_task_id: u64, target_worker: u64 }
#[derive(TraceEvent)]
struct CpuSample { timestamp_ns: u64, worker_id: u64, tid: u32, source: u8, frames: StackFrames }

fn encode_events(n: u64) -> Vec<u8> {
    let mut enc = Encoder::new();
    let mut ts: u64 = 1_000_000_000;
    for i in 0..n {
        ts += 500 + (i % 200);
        match i % 5 {
            0 => enc.write(&PollStart { timestamp_ns: ts, worker_id: i % 8, local_queue_depth: i % 32, task_id: 1000 + (i % 5000), spawn_loc_id: i % 20 }),
            1 => enc.write(&PollEnd { timestamp_ns: ts, worker_id: i % 8 }),
            2 => enc.write(&WorkerPark { timestamp_ns: ts, worker_id: i % 8, local_queue_depth: i % 16, cpu_time_ns: 500_000_000 + i * 100 }),
            3 => enc.write(&WakeEvent { timestamp_ns: ts, waker_task_id: 1000 + (i % 5000), woken_task_id: 1000 + ((i + 1) % 5000), target_worker: i % 8 }),
            _ => enc.write(&CpuSample { timestamp_ns: ts, worker_id: i % 8, tid: 12345 + (i % 4) as u32, source: 0,
                frames: StackFrames(vec![0x5555_5555_0000 + (i % 100) * 0x10, 0x5555_5555_1000 + (i % 50) * 0x20, 0x5555_5555_2000]) }),
        }
    }
    enc.finish()
}

fn format_frame(addr: u64) -> String {
    let info = resolve_symbol(addr);
    let name = info.name.unwrap_or_else(|| format!("{:#x}", addr));
    match info.code_info {
        Some(ci) => {
            let short_file = ci.file.rsplit('/').next().unwrap_or(&ci.file);
            match ci.line {
                Some(line) => format!("{name} ({short_file}:{line})"),
                None => format!("{name} ({short_file})"),
            }
        }
        None => name,
    }
}

fn main() {
    let mut sampler = PerfSampler::start(SamplerConfig {
        frequency_hz: 9999,
        event_source: EventSource::SwCpuClock,
        include_kernel: false,
    }).expect("failed to start sampler");

    for _ in 0..20 {
        let data = encode_events(1_000_000);
        std::hint::black_box(&data);
    }

    // Collect and symbolize
    let mut stacks: HashMap<Vec<String>, u64> = HashMap::new();
    sampler.for_each_sample(|s| {
        let frames: Vec<String> = s.callchain.iter().rev()
            .map(|&addr| format_frame(addr))
            .collect();
        *stacks.entry(frames).or_default() += 1;
    });

    // Print folded stacks
    let mut sorted: Vec<_> = stacks.into_iter().collect();
    sorted.sort_by(|a, b| b.1.cmp(&a.1));
    println!("=== Folded stacks ===\n");
    for (frames, count) in &sorted {
        if !frames.is_empty() {
            println!("{} {}", frames.join(";"), count);
        }
    }

    // Flat profile
    let mut flat: HashMap<String, u64> = HashMap::new();
    for (frames, count) in &sorted {
        if let Some(leaf) = frames.last() {
            *flat.entry(leaf.clone()).or_default() += count;
        }
    }
    let mut flat_sorted: Vec<_> = flat.into_iter().collect();
    flat_sorted.sort_by(|a, b| b.1.cmp(&a.1));
    let total: u64 = flat_sorted.iter().map(|(_, c)| c).sum();

    eprintln!("\n=== Flat profile ({total} samples) ===\n");
    for (sym, count) in flat_sorted.iter().take(30) {
        eprintln!("{:6.1}%  {:>5}  {}", *count as f64 / total as f64 * 100.0, count, sym);
    }
}
