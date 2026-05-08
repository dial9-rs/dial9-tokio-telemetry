---
name: dial9-runtime
description: Tokio async runtime internals reference. Covers the execution model, waking and scheduling, cooperative scheduling, poll duration effects on tail latency, worker parking, and how to connect trace data to application behavior. Use when reasoning about runtime performance from first principles.
---

# Understanding the Tokio Runtime

This skill explains how the Tokio async runtime works internally. Use this knowledge to reason about trace data, diagnose performance problems from first principles, and recommend code changes.

## The execution model

Tokio runs async tasks on a thread pool of **worker threads**. Each worker runs a loop:

1. Pick a task from its local queue (or steal from another worker, or take from the global injection queue)
2. Call `.poll()` on the task's future
3. If the future returns `Poll::Pending`, the task is suspended until something wakes it
4. If the future returns `Poll::Ready`, the task is complete
5. Repeat

A **poll** is the fundamental unit of work. During a single poll, the future runs synchronously on the worker thread until it hits an `.await` point that returns `Pending`. Nothing else can run on that worker thread until the poll completes.

### current_thread vs multi_thread

- **`new_current_thread()`**: One worker thread. All tasks share a single thread. Simple, no synchronization overhead, but all tasks compete for one CPU. Common for clients, CLIs, and lightweight servers.
- **`new_multi_thread()`**: Multiple worker threads (default: one per CPU core). Tasks can run in parallel. Uses work-stealing to balance load.

On a current_thread runtime, if one task's poll takes 500µs, every other task is blocked for that entire duration. On a multi_thread runtime with N workers, up to N tasks can be polled simultaneously.

## Waking and scheduling

When a task `.await`s something (a socket read, a timer, a channel receive), it returns `Pending` and registers a **waker**. When the awaited resource becomes ready (data arrives on the socket, timer fires, channel has a message), the waker is called, which places the task back on a worker's run queue.

The **wake-to-poll delay** is the time between `Waker::wake()` being called and the task actually being polled. This delay has two components:

1. **Queue wait**: The worker is busy polling other tasks. The woken task sits in the queue until the worker finishes its current poll and gets to it.
2. **Kernel scheduling wait**: The worker thread itself was parked (sleeping) and the OS needs to reschedule it onto a CPU core.

High wake-to-poll delays are the primary cause of tail latency in async applications. They mean "the runtime knew this task had work to do, but couldn't get to it fast enough."

## Cooperative scheduling and yield points

Tokio uses **cooperative scheduling**: a task runs until it voluntarily yields (by hitting a `Pending` `.await`). There is no preemption. If a task does CPU-heavy work or processes many items in a loop without awaiting, it monopolizes the worker.

Tokio has a built-in **coop budget** (currently 128 operations). After a task has done 128 "coop-aware" operations (socket reads, channel receives, etc.) in a single poll, Tokio forces it to yield by making the next operation return `Pending` even if data is available. This prevents a single busy task from starving others indefinitely.

However, the coop budget only applies to Tokio-aware operations. A tight loop doing synchronous work, or calling non-Tokio I/O, will not trigger a yield. In these cases, you need explicit `tokio::task::yield_now().await` calls.

### When to use yield_now()

Insert `yield_now().await` when a task processes multiple items in a loop and each iteration is fast but the total loop time could be long:

- Processing a batch of messages from a channel
- Handling pipelined requests on a connection (e.g., Redis pipelining)
- Iterating over a large data structure with periodic async work

The tradeoff: yielding adds overhead per iteration (the task must be rescheduled), which increases p50 latency slightly. But it dramatically reduces p99 latency under concurrency because other tasks get a chance to run between iterations.

**Rule of thumb**: If a single poll handles N items and each takes T microseconds, the worst-case delay for other tasks is N×T. If N×T exceeds your latency budget, add yields.

## How poll duration affects tail latency

On a single-threaded runtime with C concurrent connections, the worst-case wake-to-poll delay is approximately:

```
worst_case_delay ≈ (C - 1) × avg_poll_duration
```

On a multi-threaded runtime with W workers and C connections:

```
worst_case_delay ≈ ceil(C / W) × avg_poll_duration
```

This is why p99 latency often scales linearly with connection count when poll durations are not controlled. The fix is always the same: make individual polls shorter, either by yielding, by moving work off the runtime, or by reducing per-poll work.

### Worked example

A Redis-like server on `current_thread` with 4 connections. Each poll processes a batch of pipelined commands, taking ~225µs. When connection A's task is polled, connections B, C, and D wait. Worst case: D waits for A + B + C = 675µs. Add kernel scheduling overhead and you get p99 latencies approaching 1ms+.

Fix: `yield_now().await` after each command. Now each poll handles one command (~5-10µs of actual work + I/O await), and the maximum wait drops to ~30-40µs.

## What makes a poll long?

A poll is "long" when the future does significant work between await points. Common causes:

1. **Synchronous I/O on the runtime**: File reads, DNS resolution, blocking HTTP clients. These should use `spawn_blocking()`.
2. **CPU-intensive computation**: Serialization, compression, cryptography, large collection operations. Move to `spawn_blocking()` or break up with `yield_now()`.
3. **Lock contention**: Holding a `std::sync::Mutex` across an await, or waiting on a contended lock. Use `tokio::sync::Mutex` or restructure to avoid holding locks during polls.
4. **Batch processing without yielding**: Processing all available items in a loop (pipelined requests, channel drains, batch inserts) without giving other tasks a chance to run.
5. **Memory allocation**: Large allocations or heavy allocator contention can add microseconds to hundreds of microseconds.

In traces, look at `poll.cpuSamples` (what was on-CPU during the poll) and `poll.schedSamples` (what caused the OS to deschedule the worker — indicates blocking syscalls).

## The global injection queue

When a task is woken and the target worker's local queue is full, or when the wake comes from outside the runtime (e.g., a `spawn_blocking` completion), the task goes into the **global injection queue**. Workers periodically check this queue, but it's slower than the local queue due to synchronization.

High global queue depth (visible in `QueueSample` events) means:
- Workers can't keep up with incoming work
- Too many tasks are being woken from outside the runtime
- Workers are spending too long on individual polls

## Worker parking and unparking

When a worker has no tasks to run, it **parks** (goes to sleep via an OS futex/condvar). When new work arrives, it's **unparked** (woken by the OS). The trace captures:

- **Park → Unpark duration**: How long the worker slept. Long parks mean the worker had nothing to do.
- **CPU time on active spans**: The `ratio` field (CPU time / wall time) shows whether the worker was actually on-CPU while active. A ratio < 1.0 means the kernel descheduled the worker — it was runnable but another process/thread got the CPU.
- **schedWait on Unpark**: How long the kernel took to actually schedule the worker thread after it was woken. High values indicate CPU contention at the OS level.

## Connecting trace data to application behavior

When analyzing a trace, think in terms of this causal chain:

1. **External event** (network data arrives, timer fires) → **wake** is sent to a task
2. Task enters a worker's **run queue** → wake-to-poll delay starts
3. Worker finishes its current poll → picks up the woken task → **poll starts**
4. Future runs: reads data, processes command, writes response → **poll ends**
5. If more work is available, the task may be polled again immediately; otherwise it awaits and goes back to step 1

Latency problems map to specific parts of this chain:
- **High wake-to-poll delay**: Workers are busy (step 2-3). Fix: shorter polls, more workers, or yield points.
- **Long poll duration**: The task itself is slow (step 4). Fix: move blocking work off-runtime, add yields, optimize hot paths.
- **High kernel sched wait**: OS can't schedule workers fast enough (step 3). Fix: reduce CPU contention, check for noisy neighbors, pin workers to cores.
- **High queue depth**: More work arriving than workers can handle. Fix: add backpressure, increase worker count, optimize poll duration.

## Common fixes and their tradeoffs

| Problem | Fix | Tradeoff |
|---------|-----|----------|
| Long polls from batch processing | `yield_now().await` in the loop | Slightly higher p50 from yield overhead |
| Blocking I/O on runtime | `spawn_blocking()` | Thread pool overhead, data must be `Send` |
| CPU-heavy computation | `spawn_blocking()` or `rayon` | Same as above |
| High wake-to-poll delay, single thread | Switch to `new_multi_thread()` | More memory, synchronization overhead |
| High wake-to-poll delay, multi thread | Increase worker count or reduce poll duration | More CPU usage when idle |
| Lock contention | `tokio::sync::Mutex`, sharding, or lock-free structures | Complexity |
| Task leak (unbounded spawning) | Bounded channels, semaphores, `JoinSet` | Backpressure may slow producers |
