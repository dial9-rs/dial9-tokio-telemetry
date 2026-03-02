#!/usr/bin/env bash
set -e

cd "$(dirname "$0")/.."

echo "Building metrics-service..."
cargo build --release -p metrics-service

echo "Cleaning old traces..."
rm -f sched-trace*.bin trace_viewer/demo-trace.bin

echo "Recording demo trace..."
AWS_PROFILE="${AWS_PROFILE:-rcoh}" cargo run --release -p metrics-service --bin metrics-service -- --demo

echo "Copying to trace_viewer..."
cp sched-trace.*.bin trace_viewer/demo-trace.bin

echo "Demo trace size:"
ls -lh trace_viewer/demo-trace.bin

echo ""
echo "✓ Demo trace regenerated successfully!"
echo "  Location: trace_viewer/demo-trace.bin"
echo ""
echo "To commit:"
echo "  git add trace_viewer/demo-trace.bin"
echo "  git commit -m 'Regenerate demo trace'"
