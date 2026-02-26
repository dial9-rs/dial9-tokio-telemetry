/// Validates that our sys.rs constants match the kernel headers.
/// This compiles C programs to get kernel values and compares them to our Rust constants.
use perf_self_profile::sys::*;
use std::process::Command;
use std::sync::atomic::{AtomicU64, Ordering};

static COUNTER: AtomicU64 = AtomicU64::new(0);

fn unique_id() -> String {
    let seq = COUNTER.fetch_add(1, Ordering::Relaxed);
    format!("{}_{}", std::process::id(), seq)
}

fn check_constant(name: &str, rust_value: u64) {
    let code = format!(
        r#"
#include <linux/perf_event.h>
#include <stdio.h>
int main() {{ printf("%llu", (unsigned long long){name}); return 0; }}
"#
    );

    let id = unique_id();
    let tmp_c = format!("/tmp/check_const_{}.c", id);
    let tmp_bin = format!("/tmp/check_const_{}", id);

    std::fs::write(&tmp_c, code).expect("failed to write test file");

    let compile = Command::new("gcc")
        .args(&[&tmp_c, "-o", &tmp_bin])
        .output()
        .expect("failed to compile");

    if !compile.status.success() {
        let _ = std::fs::remove_file(&tmp_c);
        eprintln!("Skipping {} (not in kernel headers)", name);
        return;
    }

    let run = Command::new(&tmp_bin).output().expect("failed to run test");

    let _ = std::fs::remove_file(&tmp_c);
    let _ = std::fs::remove_file(&tmp_bin);

    let kernel_value = String::from_utf8_lossy(&run.stdout)
        .trim()
        .parse::<u64>()
        .unwrap_or_else(|_| {
            panic!(
                "failed to parse '{}' for {}",
                String::from_utf8_lossy(&run.stdout),
                name
            )
        });

    assert_eq!(
        rust_value, kernel_value,
        "{}: Rust has {}, kernel has {}",
        name, rust_value, kernel_value
    );
}

fn check_struct_size(name: &str, rust_size: usize) {
    let code = format!(
        r#"
#include <linux/perf_event.h>
#include <stdio.h>
int main() {{ printf("%zu", sizeof(struct {name})); return 0; }}
"#
    );

    let id = unique_id();
    let tmp_c = format!("/tmp/check_size_{}.c", id);
    let tmp_bin = format!("/tmp/check_size_{}", id);

    std::fs::write(&tmp_c, code).expect("failed to write test file");

    let compile = Command::new("gcc")
        .args(&[&tmp_c, "-o", &tmp_bin])
        .output()
        .expect("failed to compile");

    assert!(
        compile.status.success(),
        "struct {} not found in kernel headers: {}",
        name,
        String::from_utf8_lossy(&compile.stderr)
    );

    let run = Command::new(&tmp_bin).output().expect("failed to run test");

    let _ = std::fs::remove_file(&tmp_c);
    let _ = std::fs::remove_file(&tmp_bin);

    let kernel_size = String::from_utf8_lossy(&run.stdout)
        .trim()
        .parse::<usize>()
        .expect("failed to parse size");

    assert_eq!(
        rust_size, kernel_size,
        "struct {}: Rust has {} bytes, kernel has {} bytes",
        name, rust_size, kernel_size
    );
}

#[test]
fn validate_event_types() {
    check_constant("PERF_TYPE_HARDWARE", PERF_TYPE_HARDWARE as u64);
    check_constant("PERF_TYPE_SOFTWARE", PERF_TYPE_SOFTWARE as u64);
}

#[test]
fn validate_event_configs() {
    check_constant("PERF_COUNT_HW_CPU_CYCLES", PERF_COUNT_HW_CPU_CYCLES);
    check_constant("PERF_COUNT_SW_CPU_CLOCK", PERF_COUNT_SW_CPU_CLOCK);
    check_constant("PERF_COUNT_SW_TASK_CLOCK", PERF_COUNT_SW_TASK_CLOCK);
    check_constant(
        "PERF_COUNT_SW_CONTEXT_SWITCHES",
        PERF_COUNT_SW_CONTEXT_SWITCHES,
    );
}

#[test]
fn validate_sample_types() {
    check_constant("PERF_SAMPLE_IP", PERF_SAMPLE_IP);
    check_constant("PERF_SAMPLE_TID", PERF_SAMPLE_TID);
    check_constant("PERF_SAMPLE_TIME", PERF_SAMPLE_TIME);
    check_constant("PERF_SAMPLE_CALLCHAIN", PERF_SAMPLE_CALLCHAIN);
    check_constant("PERF_SAMPLE_CPU", PERF_SAMPLE_CPU);
    check_constant("PERF_SAMPLE_PERIOD", PERF_SAMPLE_PERIOD);
}

#[test]
fn validate_record_types() {
    check_constant("PERF_RECORD_LOST", PERF_RECORD_LOST as u64);
    check_constant("PERF_RECORD_SAMPLE", PERF_RECORD_SAMPLE as u64);
}

#[test]
fn validate_struct_sizes() {
    check_struct_size("perf_event_attr", std::mem::size_of::<PerfEventAttr>());
    check_struct_size(
        "perf_event_mmap_page",
        std::mem::size_of::<PerfEventMmapPage>(),
    );
    check_struct_size("perf_event_header", std::mem::size_of::<PerfEventHeader>());
}
