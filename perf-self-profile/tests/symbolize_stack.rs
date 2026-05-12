#![cfg(target_os = "linux")]

use dial9_perf_self_profile::unwinder::Unwinder;
use dial9_perf_self_profile::{read_proc_maps, resolve_symbol_with_maps};

use blazesym::symbolize::Symbolizer;

#[inline(never)]
fn outer() -> Vec<u64> {
    std::hint::black_box(inner())
}

#[inline(never)]
fn inner() -> Vec<u64> {
    let unwinder = Unwinder::install().unwrap();
    let mut buf = [0u64; 64];
    // SAFETY: handler installed above; not inside a signal handler.
    let result = unsafe { unwinder.capture(&mut buf) };
    assert!(result.frames_written >= 3, "expected at least 3 frames");
    buf[..result.frames_written].to_vec()
}

#[test]
fn capture_and_symbolize() {
    let stack = outer();
    let maps = read_proc_maps();
    let symbolizer = Symbolizer::new();

    let names: Vec<String> = stack
        .iter()
        .filter_map(|&addr| resolve_symbol_with_maps(addr, &symbolizer, &maps).name)
        .collect();

    let joined = names.join("\n");
    assert!(
        names.iter().any(|n| n.contains("inner")),
        "expected `inner` in stack:\n{joined}"
    );
    assert!(
        names.iter().any(|n| n.contains("outer")),
        "expected `outer` in stack:\n{joined}"
    );
}
