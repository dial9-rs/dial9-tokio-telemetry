#!/usr/bin/env bash
set -e

REPO_ROOT="$(git -C "$(dirname "$0")" rev-parse --show-toplevel)"
cd "$REPO_ROOT"

TRACE_PATH="$REPO_ROOT/sched-trace.bin"
DEMO_DEST="$REPO_ROOT/dial9-tokio-telemetry/trace_viewer/demo-trace.bin.gz"
# RotatingWriter turns "sched-trace.bin" into "sched-trace.0.bin", etc.
TRACE_GLOB="$REPO_ROOT/sched-trace.*.bin"

echo "Building metrics-service..."
cargo build --release -p metrics-service

echo "Cleaning old traces..."
rm -f "$TRACE_PATH" $TRACE_GLOB "$DEMO_DEST"

echo "Recording demo trace..."
AWS_PROFILE="${AWS_PROFILE:-rcoh}" cargo run --release -p metrics-service --bin metrics-service -- \
    --trace-path "$TRACE_PATH" --demo

# Demo mode uses SimpleBinaryWriter (writes directly to TRACE_PATH).
# Fall back to RotatingWriter glob if the direct file doesn't exist.
if [ -f "$TRACE_PATH" ]; then
    TRACE_FILE="$TRACE_PATH"
else
    TRACE_FILE=$(ls -1S $TRACE_GLOB 2>/dev/null | head -1)
fi

if [ -z "$TRACE_FILE" ]; then
    echo "ERROR: No trace file generated" >&2
    exit 1
fi

gzip -c "$TRACE_FILE" > "$DEMO_DEST"
rm -f "$TRACE_PATH" $TRACE_GLOB

echo "Demo trace size:"
ls -lh "$DEMO_DEST"

echo ""
echo "✓ Demo trace regenerated successfully!"
echo ""
echo "To commit:"
echo "  git add dial9-tokio-telemetry/trace_viewer/demo-trace.bin.gz"
echo "  git commit -m 'Regenerate demo trace'"
