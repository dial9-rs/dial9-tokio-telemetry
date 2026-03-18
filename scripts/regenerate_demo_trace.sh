#!/usr/bin/env bash
set -e

REPO_ROOT="$(git -C "$(dirname "$0")" rev-parse --show-toplevel)"
cd "$REPO_ROOT"

TRACE_PATH="$REPO_ROOT/sched-trace.bin"
DEMO_DEST="$REPO_ROOT/dial9-tokio-telemetry/trace_viewer/demo-trace.bin"
# RotatingWriter turns "sched-trace.bin" into "sched-trace.0.bin", "sched-trace.1.bin", etc.
TRACE_GLOB="$REPO_ROOT/sched-trace.*.bin"

echo "Building metrics-service..."
cargo build --release -p metrics-service

echo "Cleaning old traces..."
rm -f $TRACE_GLOB "$DEMO_DEST"

echo "Recording demo trace..."
AWS_PROFILE="${AWS_PROFILE:-rcoh}" cargo run --release -p metrics-service --bin metrics-service -- \
    --trace-path "$TRACE_PATH" --demo

# Find the generated trace file (rotating writer appends an index).
# The background worker may have gzip-compressed sealed segments,
# so decompress if needed before copying to the demo destination.
TRACE_FILE=$(ls -1S $TRACE_GLOB 2>/dev/null | head -1)
if [ -z "$TRACE_FILE" ]; then
    echo "ERROR: No trace file generated" >&2
    exit 1
fi

if file "$TRACE_FILE" | grep -q gzip; then
    echo "Decompressing gzipped trace..."
    gunzip -c "$TRACE_FILE" > "$DEMO_DEST"
else
    cp "$TRACE_FILE" "$DEMO_DEST"
fi
rm -f $TRACE_GLOB

echo "Demo trace size:"
ls -lh "$DEMO_DEST"

echo ""
echo "✓ Demo trace regenerated successfully!"
echo ""
echo "To commit:"
echo "  git add dial9-tokio-telemetry/trace_viewer/demo-trace.bin"
echo "  git commit -m 'Regenerate demo trace'"
