# Agent Guidelines

## Demo Trace

If you modify the trace format (event structure, encoding, parser, etc.), you MUST regenerate the demo trace:

```bash
./scripts/regenerate_demo_trace.sh
```

Or manually:

```bash
cd dial9-tokio-telemetry
rm -f trace_viewer/demo-trace.bin
cargo build --release -p metrics-service
AWS_PROFILE=your-profile cargo run --release -p metrics-service --bin metrics-service -- --trace-path sched-trace.bin --demo
cp sched-trace.*.bin trace_viewer/demo-trace.bin
git add trace_viewer/demo-trace.bin
git commit -m "Regenerate demo trace after format changes"
```

The demo trace is used for:
- Live demos on the hosted viewer
- Documentation screenshots
- Testing the viewer with real data

Failing to update it will cause the viewer to fail when loading the demo.

## Meta

- Never declare done after pushing or opening a PR until CI is green. Check CI status and fix any failures before moving on.
