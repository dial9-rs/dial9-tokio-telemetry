# Tokio Telemetry Trace Analysis Guide

## Quick Start

```bash
# 1. Get basic statistics
cargo run --example analyze_trace -- trace.0.bin

# 2. Convert to JSONL for custom analysis
cargo run --example trace_to_jsonl -- trace.0.bin trace.jsonl

# 3. View CPU profile
cargo run --example cpu_flamegraph -- trace.0.bin > flamegraph.txt
```

## Understanding the Output

### Key Metrics from `analyze_trace`

- **Wake→Poll delays**: Time from when a task is woken until it runs
  - High p99 (>10ms) indicates scheduling issues
  - Check local queue depths when delays are high
- **Poll durations**: Time spent executing task code
  - High values indicate CPU-bound work
- **Worker utilization**: Busy vs parked time
  - High utilization + high delays = queue buildup

### Spawn Locations

Tasks are grouped by where they were spawned:
- Look for locations with many polls and high average poll time
- These consume the most CPU

## Diagnosing High Latency

### Step 1: Identify the Problem Tasks

Find tasks with high wake→poll delays:

```python
# Find tasks with >10ms delays
import json
from collections import defaultdict

task_wakes = defaultdict(list)
task_polls = defaultdict(list)

with open('trace.jsonl') as f:
    for line in f:
        e = json.loads(line)
        if e['event'] == 'WakeEvent':
            task_wakes[e['woken_task_id']].append(e['timestamp_ns'])
        elif e['event'] == 'PollStart':
            task_polls[e['task_id']].append({
                'ts': e['timestamp_ns'],
                'worker': e['worker'],
                'loc': e.get('spawn_loc_id'),
                'local_q': e['local_q']
            })

# Match wakes to polls and find delays
for task_id in task_polls:
    wakes = sorted(task_wakes.get(task_id, []))
    polls = sorted(task_polls[task_id], key=lambda x: x['ts'])
    # ... calculate delays
```

**Key insight**: Check `local_q` (local queue depth) when delays are high. Values >20 indicate queue buildup.

### Step 2: Find What's Blocking

For each high-delay period, count what the worker was doing:

```python
# During delay period (wake_ts to poll_ts on worker W):
# - Count PollStart events on that worker
# - Group by spawn_loc_id to see which task types dominated
```

**Pattern**: If worker polled 300+ tasks during a 100ms delay, it was busy with other work.

### Step 3: Identify CPU Hotspots

Use CPU samples to see what code was running during blocking:

```rust
// See examples/blocked_analysis.rs for full implementation
// 1. Find blocked periods (wake→poll > threshold)
// 2. Filter CPU samples to those time ranges on those workers
// 3. Aggregate stack traces into flamegraph format
```

**Look for**:
- Expensive operations (crypto, parsing, serialization)
- Specific libraries dominating samples
- Unexpected synchronous work

## Common Patterns

### Pattern 1: Local Queue Buildup

**Symptoms**:
- High wake→poll delays (>50ms p99)
- High local queue depths (>30)
- Workers are busy (>70% utilization)
- Global queue depth is 0

**Cause**: Tasks spawned on one worker stay there, creating imbalance.

**Solution**: 
- Reduce task spawn rate
- Use `spawn_blocking` for CPU-heavy work
- Tune `global_queue_interval` to force work distribution

### Pattern 2: Priority Inversion

**Symptoms**:
- High-priority tasks (e.g., request handlers) have high delays
- CPU samples show low-priority work (e.g., background clients)
- Specific spawn locations dominate CPU during blocking

**Cause**: FIFO local queues don't prioritize important work.

**Solution**:
- Separate runtimes for different priority levels
- Reduce concurrency of background tasks
- Use semaphores to limit concurrent background work

### Pattern 3: CPU-Bound Tasks

**Symptoms**:
- High average poll durations (>500µs)
- CPU samples concentrated in specific functions
- Low wake→poll delays

**Cause**: Tasks doing too much work per poll.

**Solution**:
- Move expensive work to `spawn_blocking`
- Add yield points in long-running tasks
- Optimize hot functions shown in CPU samples

## Useful Scripts

### Find Tasks by Spawn Location

```bash
# Get spawn location mapping
cargo run --example analyze_trace -- trace.0.bin 2>&1 | grep "=== Spawn Locations"
```

### Aggregate CPU by Function

```bash
# Get flamegraph-style output
cargo run --example cpu_flamegraph -- trace.0.bin | \
  grep "metrics_service\|aws_" | \
  head -20
```

### Check Worker Balance

```python
# Count polls per worker
import json
from collections import Counter

worker_polls = Counter()
with open('trace.jsonl') as f:
    for line in f:
        e = json.loads(line)
        if e['event'] == 'PollStart':
            worker_polls[e['worker']] += 1

# Should be roughly equal (within 10%)
```

## Tips

1. **Start with `analyze_trace`** - gives you the big picture
2. **Check wake→poll delays first** - usually the root cause of p99 issues
3. **Local queue depth matters** - values >20 indicate problems
4. **CPU samples are sparse** - you need many blocked periods to get good data
5. **Match spawn locations to code** - use line numbers to find the actual code
6. **Workers should be balanced** - uneven poll counts indicate work stealing issues

## Example Analysis Workflow

```bash
# 1. Get overview
cargo run --example analyze_trace -- trace.0.bin

# 2. See high wake→poll delays? Check local queues
#    Write script to correlate delays with queue depths

# 3. Find what's blocking
#    Write script to count polls during high-delay periods

# 4. Get CPU profile of blocking work
cargo run --example blocked_analysis

# 5. Identify root cause from CPU samples
#    Look for expensive operations or unexpected work
```
