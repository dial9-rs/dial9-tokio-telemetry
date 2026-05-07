# Trace Diagnosis Benchmark

Manual testing harness for verifying dial9 skills work correctly with AI agents.

## Quick start

```bash
# Generate target project, install skills, run agent, capture output
./run.sh

# Or with explicit target directory
./run.sh /path/to/target

# Regenerate from scratch
./run.sh --clean
```

## What it does

1. `setup.sh`: generates a minimal Rust project with `dial9-tokio-telemetry` as a dep, copies the demo trace, installs skills via Symposium (falls back to `dial9-viewer agents skills` if symposium isn't available)
2. `run.sh`: calls setup if needed, then runs `claude -p` with a diagnosis prompt and captures output

## Evaluating results

After the run completes, evaluate the output against `EXPECTED.md`:

```
Evaluate /tmp/dial9-skill-benchmark-*.md against benchmarks/trace-diagnosis/EXPECTED.md
```

## Prerequisites

- `dial9-viewer` built from this repo (or installed)
- `claude` CLI authenticated
- Optionally: `symposium` installed for skill sync (otherwise falls back to direct unpack)

## What it tests

- Skills are discoverable by the agent
- Agent uses the toolkit scripts to analyze the demo trace
- Agent follows the diagnostic workflow (red flags first, then drill down)
- Agent produces actionable, trace-backed recommendations
- Agent does not hallucinate data or skip analysis
- Agent recovers from tool failures (API misuse, stack overflows, missing files) without getting stuck
