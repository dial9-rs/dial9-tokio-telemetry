use perf_self_profile::{resolve_symbol, EventSource, PerfSampler, SamplerConfig};
use std::sync::{Arc, Mutex};
use std::thread;

fn main() {
    let sampler = Arc::new(Mutex::new(
        PerfSampler::new_per_thread(SamplerConfig {
            frequency_hz: 1,
            event_source: EventSource::SwContextSwitches,
            include_kernel: false,
        })
        .unwrap(),
    ));
    sampler.lock().unwrap().track_current_thread().unwrap();
    thread::sleep(std::time::Duration::from_millis(50));

    let mut sampler = sampler.lock().unwrap();
    sampler.disable();
    let samples = sampler.drain_samples();
    println!("{} samples", samples.len());
    for s in &samples {
        println!(
            "  ip={:#018x} tid={} callchain ({} frames):",
            s.ip,
            s.tid,
            s.callchain.len()
        );
        for (i, &addr) in s.callchain.iter().enumerate() {
            println!("    [{:2}] {:#018x} {:?}", i, addr, resolve_symbol(addr));
        }
    }
}
