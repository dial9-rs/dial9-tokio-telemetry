---
name: dial9-runtime
description: Tokio async runtime internals reference. Covers the execution model, waking and scheduling, cooperative scheduling, poll duration effects on tail latency, worker parking, and how to connect trace data to application behavior. Use when reasoning about runtime performance from first principles.
---

# Understanding the Tokio Runtime

## The execution model

Tokio runs async tasks on a thread pool of **worker threads**. Each worker runs a loop:

1. Pick a task from its local queue (or steal from another worker, or take from the global injection queue)
2. Call `.poll()` on the task's future
3. If the future returns `Poll::Pending`, the task is suspended until something wakes it
4. If the future returns `Poll::Ready`, the task is complete
5. Repeat

A **poll** is the fundamental unit of work. During a single poll, the future runs synchronously on the worker thread until it hits an `.await` that returns `Pending`. Nothing else can run on that worker until the poll completes.

### current_thread vs multi_thread

- **`new_current_thread()`**: One worker thread. All tasks share a single thread. Common for clients, CLIs, and lightweight servers.
- **`new_multi_thread()`**: Multiple worker threads (default: one per CPU core). Tasks can run in parallel via work-stealing.

## Waking and scheduling

When a task `.await`s something, it returns `Pending` and registers a **waker**. When the awaited resource becomes ready, the waker places the task back on a worker's run queue.

The **wake-to-poll delay** is the time between `Waker::wake()` and the task being polled. Two components:

1. **Queue wait**: The worker is busy polling other tasks.
2. **Kernel scheduling wait**: The worker thread was parked and the OS needs to reschedule it.

High wake-to-poll delays are the primary cause of tail latency.

## Cooperative scheduling and yield points

Tokio uses **cooperative scheduling**: a task runs until it voluntarily yields. There is no preemption.

Tokio's built-in **coop budget** (128 operations) forces a yield after 128 coop-aware operations. But synchronous work or non-Tokio I/O won't trigger it. Use `tokio::task::yield_now().await` for those cases.

**When to yield**: If a single poll handles N items each taking T microseconds, worst-case delay for other tasks is N*T. If that exceeds your latency budget, add yields.

## How poll duration affects tail latency

On a single-threaded runtime with C concurrent connections:
```
worst_case_delay ≈ (C - 1) × avg_poll_duration
```

On a multi-threaded runtime with W workers:
```
worst_case_delay ≈ ceil(C / W) × avg_poll_duration
```

p99 latency scales linearly with connection count when poll durations are uncontrolled.

## What makes a poll long?

1. **Synchronous I/O**: File reads, DNS resolution, blocking HTTP clients. Use `spawn_blocking()`.
2. **CPU-intensive computation**: Serialization, compression, crypto. Use `spawn_blocking()` or `rayon`.
3. **Lock contention**: Holding `std::sync::Mutex` across an await. Use `tokio::sync::Mutex`.
4. **Batch processing without yielding**: Processing all items in a loop without giving other tasks a chance.
5. **Memory allocation**: Large allocations or heavy allocator contention.

In traces, check `poll.cpuSamples` (what was on-CPU) and `poll.schedSamples` (blocking syscalls).

## The global injection queue

When a task is woken and the target worker's local queue is full, or the wake comes from outside the runtime, the task goes into the global injection queue. High depth means workers cannot keep up.

## Worker parking and unparking

When a worker has no tasks, it parks (sleeps). Trace data shows:
- **Park duration**: How long the worker slept.
- **CPU ratio on active spans**: < 1.0 means the kernel descheduled the worker.
- **schedWait on Unpark**: Kernel scheduling latency after wake.

## Connecting trace data to application behavior

Causal chain:
1. External event → **wake** sent to a task
2. Task enters run queue → wake-to-poll delay starts
3. Worker finishes current poll → picks up task → **poll starts**
4. Future runs → **poll ends**
5. Task awaits again or completes

Latency problems map to:
- **High wake-to-poll delay**: Workers busy. Fix: shorter polls, more workers, yield points.
- **Long poll duration**: Task itself is slow. Fix: move blocking work off-runtime, add yields.
- **High kernel sched wait**: OS cannot schedule workers. Fix: reduce CPU contention.
- **High queue depth**: More work than workers can handle. Fix: backpressure, more workers.

## Common fixes

| Problem | Fix | Tradeoff |
|---------|-----|----------|
| Batch processing without yielding | `yield_now().await` | Slightly higher p50 |
| Blocking I/O on runtime | `spawn_blocking()` | Thread pool overhead |
| CPU-heavy computation | `spawn_blocking()` or `rayon` | Data must be `Send` |
| High wake-to-poll, single thread | Switch to `new_multi_thread()` | More memory |
| Lock contention | `tokio::sync::Mutex`, sharding | Complexity |
| Task leak | Bounded channels, semaphores, `JoinSet` | Backpressure |
