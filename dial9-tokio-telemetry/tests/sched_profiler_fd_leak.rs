//! Integration test: sched profiler should only track worker threads, not blocking pool threads.
//!
//! Before the fix, every thread that started (including blocking pool threads) opened a
//! perf_event_open fd + 2 MB mmap ring buffer for sched event sampling. With Tokio's default
//! blocking pool limit of 512 threads, this could exhaust file descriptors.
//!
//! Additionally, `stop_tracking_current_thread` never cleaned up fds because `open_perf_event`
//! stored `tid: 0` (the "current thread" sentinel) while the cleanup searched for `gettid()`.

#![cfg(all(feature = "cpu-profiling", target_os = "linux"))]

mod common;

use dial9_tokio_telemetry::telemetry::TracedRuntime;
use dial9_tokio_telemetry::telemetry::cpu_profile::SchedEventConfig;
use std::sync::Mutex;
use std::time::Duration;

/// Serialize tests that inspect process-wide perf_event fd counts.
/// `cargo test` runs tests in the same binary in parallel (threads),
/// so concurrent tests would see each other's fds.
static PERF_FD_TEST_LOCK: Mutex<()> = Mutex::new(());

/// Count open perf_event fds specifically.
fn count_perf_fds() -> usize {
    std::fs::read_dir("/proc/self/fd")
        .expect("failed to read /proc/self/fd")
        .filter_map(|e| e.ok())
        .filter(|e| {
            std::fs::read_link(e.path())
                .map(|p| p.to_string_lossy().contains("perf_event"))
                .unwrap_or(false)
        })
        .count()
}

/// Spawning many blocking threads should NOT cause fd count to grow proportionally.
///
/// With the bug, each `spawn_blocking` call opens a perf fd that is never reclaimed.
/// After the fix, only worker threads (a fixed, small number) get perf fds.
#[test]
fn sched_profiler_fds_bounded_with_many_blocking_threads() {
    let _lock = PERF_FD_TEST_LOCK.lock().unwrap();
    let (writer, _events) = common::CapturingWriter::new();

    let num_workers = 2;
    let num_blocking_tasks = 50;

    let mut builder = tokio::runtime::Builder::new_multi_thread();
    builder.worker_threads(num_workers).enable_all();

    let (runtime, guard) = TracedRuntime::builder()
        .with_sched_events(SchedEventConfig::default())
        .build_and_start(builder, writer)
        .unwrap();

    // Let workers start and resolve their identity.
    runtime.block_on(async {
        tokio::time::sleep(Duration::from_millis(100)).await;
    });

    let perf_fds_before = count_perf_fds();

    // Spawn many blocking tasks. Each one creates a new blocking pool thread.
    // Use std::thread::sleep to ensure they actually block and force new threads.
    runtime.block_on(async {
        let mut handles = Vec::new();
        for _ in 0..num_blocking_tasks {
            handles.push(tokio::task::spawn_blocking(|| {
                std::thread::sleep(Duration::from_millis(50));
            }));
        }
        for h in handles {
            h.await.unwrap();
        }
        // Wait for threads to exit and on_thread_stop to fire.
        tokio::time::sleep(Duration::from_millis(500)).await;
    });

    let perf_fds_after = count_perf_fds();

    drop(runtime);
    drop(guard);

    // Only worker threads should have perf fds. Before the fix, we'd see
    // ~50 new perf fds (one per blocking thread). After the fix, the count
    // should stay at exactly num_workers.
    assert_eq!(
        perf_fds_before, perf_fds_after,
        "perf fd count changed from {perf_fds_before} to {perf_fds_after} after \
         spawning {num_blocking_tasks} blocking tasks. \
         Sched profiler is likely opening fds for blocking pool threads."
    );
}

/// Verify that sched profiler fds are properly cleaned up when the runtime shuts down.
///
/// This catches the tid=0 bug where `stop_tracking_current_thread` can never find
/// the event to remove because `open_perf_event` stored tid=0 instead of the real tid.
#[test]
fn sched_profiler_fds_cleaned_up_on_shutdown() {
    let _lock = PERF_FD_TEST_LOCK.lock().unwrap();
    assert_eq!(count_perf_fds(), 0, "no perf fds should exist before test");

    {
        let (writer, _events) = common::CapturingWriter::new();

        let num_workers = 4;
        let mut builder = tokio::runtime::Builder::new_multi_thread();
        builder.worker_threads(num_workers).enable_all();

        let (runtime, guard) = TracedRuntime::builder()
            .with_sched_events(SchedEventConfig::default())
            .build_and_start(builder, writer)
            .unwrap();

        // Do some work so workers resolve their identity.
        runtime.block_on(async {
            for _ in 0..10 {
                tokio::spawn(async { tokio::task::yield_now().await })
                    .await
                    .unwrap();
            }
            tokio::time::sleep(Duration::from_millis(200)).await;
        });

        // Workers should have perf fds while running.
        let perf_fds_during = count_perf_fds();
        assert!(
            perf_fds_during > 0,
            "expected perf fds while runtime is running, got 0"
        );

        drop(runtime);
        drop(guard);
    }

    let perf_fds_after = count_perf_fds();
    assert_eq!(
        perf_fds_after, 0,
        "leaked {perf_fds_after} perf_event fds after runtime shutdown. \
         stop_tracking_current_thread likely failed to find events due to tid=0 bug."
    );
}
