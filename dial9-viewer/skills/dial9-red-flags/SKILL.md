---
name: dial9-red-flags
description: Automated health checks for dial9 Tokio runtime traces. Detects long polls, task leaks, scheduling delays, blocking calls, queue buildup, worker imbalance, CPU contention, and span anomalies. Use when you want a quick automated assessment of trace health.
---

# Red Flags: Automated Health Checks

Run `scripts/red_flag_scan.js` against any trace to surface common Tokio runtime problems.

```bash
node scripts/red_flag_scan.js <trace.bin or directory>
```

Each finding has a severity: critical, warning, or info.

## Checks performed

### long-poll
A single `.poll()` call took too long (>10ms warning, >50ms critical). This blocks the worker from processing other tasks. Look at `poll.cpuSamples` and `poll.schedSamples` for stack traces.

### task-leak
Active task count grows without bound. Tasks are spawned but never complete. Check `taskSpawnLocs` for spawn locations of unterminated tasks.

### sched-delay
Time between `Waker::wake()` and the task being polled exceeds 5ms. All workers are busy. Fix: shorter polls, more workers, or yield points.

### blocking-calls
Scheduling samples (source=1) reveal blocking system calls on the async runtime. These should use `spawn_blocking` or a dedicated thread.

### queue-depth
Global injection queue exceeds 100 (warning) or 1000 (critical). The runtime cannot keep up with incoming work.

### worker-imbalance
Poll counts differ by more than 3x across workers. Work-stealing may not be distributing evenly, or one worker is stuck on long polls.

### cpu-contention
Workers are active but spending less than 50% of wall time on CPU. The kernel is descheduling them due to CPU contention.

### kernel-sched-wait
Worker unpark takes more than 1ms of kernel scheduling wait. Indicates CPU contention at the OS level.

### many-spans-per-poll
A single poll contains more than 20 span enter/exit pairs. Usually a tight loop without yielding.

### span-duration-outlier
A span whose duration exceeds 10x the P50 for its name. Flags individual slow operations.

### unmatched-spans
Spans with enter but no exit. Small counts are normal at segment boundaries. Large counts may indicate task cancellation.
