use dial9_perf_self_profile::{EventSource, PerfSampler, SamplerConfig, resolve_symbol};
use std::sync::{Arc, Mutex};
use std::thread;

#[inline(never)]
fn do_sleep() {
    thread::sleep(std::time::Duration::from_millis(50));
}

fn main() {
    unsafe { libc::prctl(libc::PR_SET_DUMPABLE, 1) };
    let sampler = Arc::new(Mutex::new(
        PerfSampler::new_per_thread(SamplerConfig {
            frequency_hz: 1,
            event_source: EventSource::SwContextSwitches,
            include_kernel: false,
        })
        .unwrap(),
    ));
    sampler.lock().unwrap().track_current_thread().unwrap();
    do_sleep();

    let mut sampler = sampler.lock().unwrap();
    sampler.disable();
    let samples = sampler.drain_samples();
    println!("{} samples", samples.len());
    for s in &samples {
        println!("  tid={} callchain ({} frames):", s.tid, s.callchain.len());
        for (i, &addr) in s.callchain.iter().enumerate() {
            let info = resolve_symbol(addr);
            println!("    [{:2}] {:#018x} {:?}", i, addr, info.name);
        }
    }
}
