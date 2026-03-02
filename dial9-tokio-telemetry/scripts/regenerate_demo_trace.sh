#!/usr/bin/env bash
set -e

REPO_ROOT="$(git -C "$(dirname "$0")" rev-parse --show-toplevel)"
cd "$REPO_ROOT"

TRACE_PATH="$REPO_ROOT/sched-trace.bin"
DEMO_DEST="$REPO_ROOT/dial9-tokio-telemetry/trace_viewer/demo-trace.bin"

echo "Building metrics-service..."
cargo build --release -p metrics-service

echo "Cleaning old traces..."
rm -f "$TRACE_PATH"*.bin "$DEMO_DEST"

echo "Recording demo trace..."
AWS_PROFILE="${AWS_PROFILE:-rcoh}" cargo run --release -p metrics-service --bin metrics-service -- \
    --trace-path "$TRACE_PATH" --demo

# Find the generated trace file (rotating writer appends an index)
TRACE_FILE=$(ls -1 "$TRACE_PATH".*.bin 2>/dev/null | head -1)
if [ -z "$TRACE_FILE" ]; then
    echo "ERROR: No trace file generated" >&2
    exit 1
fi

cp "$TRACE_FILE" "$DEMO_DEST"
rm -f "$TRACE_PATH"*.bin

echo "Demo trace size:"
ls -lh "$DEMO_DEST"

echo ""
echo "✓ Demo trace regenerated successfully!"
echo ""
echo "To commit:"
echo "  git add dial9-tokio-telemetry/trace_viewer/demo-trace.bin"
echo "  git commit -m 'Regenerate demo trace'"
