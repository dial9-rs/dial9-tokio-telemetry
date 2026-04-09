#!/usr/bin/env bash
set -e

FLAG_PROFILE=""
while [[ $# -gt 0 ]]; do
    case "$1" in
        --aws-profile=*) FLAG_PROFILE="${1#*=}"; shift ;;
        --aws-profile) FLAG_PROFILE="$2"; shift 2 ;;
        *) echo "Unknown option: $1" >&2; exit 1 ;;
    esac
done

if [ -n "$FLAG_PROFILE" ]; then
    export AWS_PROFILE="$FLAG_PROFILE"
elif [ -z "$AWS_PROFILE" ]; then
    echo "Error: No AWS profile specified." >&2
    echo "Either pass --aws-profile=<profile> or set the AWS_PROFILE environment variable." >&2
    exit 1
fi

REPO_ROOT="$(git -C "$(dirname "$0")" rev-parse --show-toplevel)"
cd "$REPO_ROOT"

TRACE_PATH="$REPO_ROOT/sched-trace.bin"
DEMO_DEST="$REPO_ROOT/dial9-viewer/ui/demo-trace.bin"
# RotatingWriter turns "sched-trace.bin" into "sched-trace.0.bin.gz", etc.
TRACE_GZ_GLOB="$REPO_ROOT/sched-trace.*.bin.gz"

echo "Building metrics-service..."
cargo build --release -p metrics-service

echo "Cleaning old traces..."
rm -f $TRACE_GZ_GLOB "$DEMO_DEST"

echo "Recording demo trace..."
cargo run --release -p metrics-service --bin metrics-service -- \
    --trace-path "$TRACE_PATH" --demo

# Find the generated trace file (rotating writer appends an index).
TRACE_FILE=$(ls -1S $TRACE_GZ_GLOB 2>/dev/null | head -1)
if [ -z "$TRACE_FILE" ]; then
    echo "ERROR: No trace file generated" >&2
    exit 1
fi

gunzip -c "$TRACE_FILE" > "$DEMO_DEST"
rm -f $TRACE_GZ_GLOB

echo "Demo trace size:"
ls -lh "$DEMO_DEST"

echo ""
echo "✓ Demo trace regenerated successfully!"
echo ""
echo "To commit:"
echo "  git add dial9-viewer/ui/demo-trace.bin"
echo "  git commit -m 'Regenerate demo trace'"
