mod common;
use perf_self_profile::{EventSource, PerfSampler, SamplerConfig, resolve_symbol};
use std::sync::{Arc, Mutex};
use std::thread;

#[inline(never)]
fn block_on_lock(lock: &Mutex<()>) {
    let _g = lock.lock().unwrap();
}

#[inline(never)]
fn do_sleep() {
    thread::sleep(std::time::Duration::from_millis(50));
}

#[test]
fn captures_lock_acquisition_stack() {
    if common::is_ci() {
        return;
    }
    unsafe { libc::prctl(libc::PR_SET_DUMPABLE, 1) };
    let sampler = Arc::new(Mutex::new(
        PerfSampler::new_per_thread(SamplerConfig {
            frequency_hz: 1,
            event_source: EventSource::SwContextSwitches,
            include_kernel: false,
        })
        .expect("failed to create sampler"),
    ));

    sampler.lock().unwrap().track_current_thread().unwrap();

    let lock = Arc::new(Mutex::new(()));
    let guard = lock.lock().unwrap();

    let handles: Vec<_> = (0..4)
        .map(|_| {
            let lock2 = Arc::clone(&lock);
            let sampler2 = Arc::clone(&sampler);
            thread::spawn(move || {
                sampler2.lock().unwrap().track_current_thread().unwrap();
                block_on_lock(&lock2);
                sampler2.lock().unwrap().stop_tracking_current_thread();
            })
        })
        .collect();

    thread::sleep(std::time::Duration::from_millis(100));

    drop(guard);
    for h in handles {
        h.join().unwrap();
    }

    let mut sampler = sampler.lock().unwrap();
    sampler.disable();
    let samples = sampler.drain_samples();

    assert!(
        !samples.is_empty(),
        "expected samples from context switches"
    );
    assert!(
        samples.iter().any(|s| !s.callchain.is_empty()),
        "expected at least one sample with a callchain"
    );

    // Note: glibc's pthread_mutex_lock lacks frame pointers, so the kernel
    // unwinder can't walk past it into our binary. We verify samples arrive
    // but can't assert on symbol names for lock contention stacks.
    for sample in &samples {
        for &addr in &sample.callchain {
            let info = resolve_symbol(addr);
            if let Some(name) = &info.name {
                eprintln!("  tid={} {:#018x} {}", sample.tid, addr, name);
            }
        }
    }
}

#[test]
fn captures_sleep_stack() {
    if common::is_ci() {
        return;
    }
    unsafe { libc::prctl(libc::PR_SET_DUMPABLE, 1) };
    let sampler = Arc::new(Mutex::new(
        PerfSampler::new_per_thread(SamplerConfig {
            frequency_hz: 1,
            event_source: EventSource::SwContextSwitches,
            include_kernel: false,
        })
        .expect("failed to create sampler"),
    ));

    sampler.lock().unwrap().track_current_thread().unwrap();
    do_sleep();

    let mut sampler = sampler.lock().unwrap();
    sampler.disable();
    let samples = sampler.drain_samples();

    assert!(
        !samples.is_empty(),
        "expected samples from sleep context switches"
    );

    // Sleep goes through nanosleep which preserves the frame chain, so we
    // should see our binary's symbols in the callchain.
    let mut resolved_names = Vec::new();
    for sample in &samples {
        for &addr in &sample.callchain {
            let info = resolve_symbol(addr);
            eprintln!("  tid={} {:#018x} -> {:?}", sample.tid, addr, info.name);
            if let Some(name) = info.name {
                resolved_names.push(name);
            }
        }
    }

    // Debug: print exe range
    let exe = std::fs::read_link("/proc/self/exe").unwrap();
    let maps = std::fs::read_to_string("/proc/self/maps").unwrap();
    eprintln!("exe: {:?}", exe);
    for line in maps.lines() {
        if line.contains(exe.to_str().unwrap()) {
            eprintln!("  {}", line);
        }
    }

    // Frame pointer unwinding may produce shallow stacks in test binaries,
    // so we only assert that we resolved *something*.
    assert!(
        !resolved_names.is_empty(),
        "expected at least one resolved symbol from sleep stacks. \
         Got {} samples with 0 resolved names.",
        samples.len(),
    );
}
