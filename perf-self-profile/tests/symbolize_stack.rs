#![cfg(target_os = "linux")]

use dial9_perf_self_profile::unwinder::Unwinder;
use dial9_perf_self_profile::{read_proc_maps, resolve_symbol_with_maps};

use blazesym::symbolize::Symbolizer;

#[inline(never)]
fn outer(unwinder: &Unwinder) -> Vec<u64> {
    std::hint::black_box(inner(unwinder))
}

#[inline(never)]
fn inner(unwinder: &Unwinder) -> Vec<u64> {
    let mut buf = [0u64; 64];
    // SAFETY: handler installed by caller; not inside a signal handler.
    let result = unsafe { unwinder.capture(&mut buf) };
    assert!(result.frames_written >= 3, "expected at least 3 frames");
    buf[..result.frames_written].to_vec()
}

#[test]
fn capture_and_symbolize() {
    let unwinder = Unwinder::install().unwrap();
    if !unwinder.verify_handler() {
        eprintln!("skipping: SIGSEGV handler was replaced (sanitizer?)");
        return;
    }

    let stack = outer(&unwinder);
    let maps = read_proc_maps();
    let symbolizer = Symbolizer::new();

    let names: Vec<String> = stack
        .iter()
        .filter_map(|&addr| resolve_symbol_with_maps(addr, &symbolizer, &maps).name)
        .collect();
    assert_eq!(
        names[0..3],
        vec![
            "symbolize_stack::inner",
            "symbolize_stack::outer",
            "symbolize_stack::capture_and_symbolize"
        ]
    );
}
