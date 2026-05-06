//! Benchmark for the frame-pointer unwinder.
//!
//! Measures latency of walking a real frame-pointer chain of known depth.
//! Run with: RUSTFLAGS="-C force-frame-pointers=yes" cargo bench --bench unwind --features __internal-bench

#![cfg(all(target_os = "linux", target_arch = "x86_64"))]

use std::arch::asm;
use std::hint::black_box;

use criterion::{Criterion, criterion_group, criterion_main};
use dial9_perf_self_profile::__bench_internals::{install_handler, unwind};

/// Read the current frame pointer (rbp), stack pointer (rsp), and instruction pointer (rip).
#[inline(always)]
fn read_registers() -> (usize, usize, usize) {
    let fp: usize;
    let sp: usize;
    let pc: usize;
    unsafe {
        asm!(
            "mov {fp}, rbp",
            "mov {sp}, rsp",
            "lea {pc}, [rip]",
            fp = out(reg) fp,
            sp = out(reg) sp,
            pc = out(reg) pc,
        );
    }
    (pc, fp, sp)
}

/// Perform the unwind and return the number of frames walked.
#[inline(never)]
fn do_unwind() -> usize {
    let (pc, fp, sp) = read_registers();
    let mut out = [0u64; 128];
    unsafe { unwind(pc, fp, sp, &mut out) }
}

// Build a chain of exactly N inline(never) frames via recursion.
#[inline(never)]
fn recurse(depth: u32) -> usize {
    if depth == 0 {
        black_box(do_unwind())
    } else {
        black_box(recurse(depth - 1))
    }
}

fn bench_unwind_20(c: &mut Criterion) {
    unsafe { install_handler().expect("failed to install SIGSEGV handler") };

    // Verify we actually get enough frames
    let frames = recurse(20);
    assert!(
        frames >= 15,
        "expected at least 15 frames from 20-deep recursion, got {frames}"
    );
    eprintln!("20-frame bench: unwinder walked {frames} frames");

    c.bench_function("unwind_20_frames", |b| {
        b.iter(|| black_box(recurse(20)));
    });
}

fn bench_unwind_5(c: &mut Criterion) {
    // Verify we get frames
    let frames = recurse(5);
    assert!(
        frames >= 4,
        "expected at least 4 frames from 5-deep recursion, got {frames}"
    );
    eprintln!("5-frame bench: unwinder walked {frames} frames");

    c.bench_function("unwind_5_frames", |b| {
        b.iter(|| black_box(recurse(5)));
    });
}

criterion_group!(benches, bench_unwind_20, bench_unwind_5);
criterion_main!(benches);
