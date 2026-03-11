#!/usr/bin/env bash
# Stress test: can the S3 background worker keep up with a 64-worker runtime
# producing trace segments at high throughput?
#
# Usage:
#   S3_STRESS_BUCKET=my-bucket ./scripts/stress-test-s3-worker.sh
#
# Optional env vars:
#   AWS_REGION       — defaults to us-east-1
#   WORKER_THREADS   — defaults to 64
#   RUN_DURATION     — seconds to run, defaults to 60
#   SEGMENT_SIZE     — bytes per segment before rotation, defaults to 262144 (256KB)
#   TOTAL_SIZE       — max total disk, defaults to 10MB
set -euo pipefail

BUCKET="${S3_STRESS_BUCKET:?Set S3_STRESS_BUCKET to an existing bucket}"
REGION="${AWS_REGION:-us-east-1}"
WORKERS="${WORKER_THREADS:-64}"
DURATION="${RUN_DURATION:-60}"
SEGMENT_SIZE="${SEGMENT_SIZE:-262144}"
TOTAL_SIZE="${TOTAL_SIZE:-10485760}"

REPO_ROOT="$(git -C "$(dirname "$0")" rev-parse --show-toplevel)"
cd "$REPO_ROOT"

TRACE_DIR=$(mktemp -d /tmp/dial9-stress-XXXXXX)
TRACE_PATH="$TRACE_DIR/trace.bin"
PREFIX="stress-test/$(date -u +%Y-%m-%dT%H%M%S)"

cleanup() {
    rm -rf "$TRACE_DIR"
}
trap cleanup EXIT

echo "=== S3 Worker Stress Test ==="
echo "  Bucket:         $BUCKET"
echo "  Region:         $REGION"
echo "  Workers:        $WORKERS"
echo "  Duration:       ${DURATION}s"
echo "  Segment size:   $SEGMENT_SIZE bytes"
echo "  Total disk:     $TOTAL_SIZE bytes"
echo "  S3 prefix:      $PREFIX"
echo ""

# Build
echo "Building stress test binary..."
cargo build --release -p dial9-tokio-telemetry --example s3_stress_test

echo "Running stress test..."
RUST_LOG=info,dial9_worker=debug \
  cargo run --release -p dial9-tokio-telemetry --example s3_stress_test -- \
    --trace-path "$TRACE_PATH" \
    --bucket "$BUCKET" \
    --prefix "$PREFIX" \
    --region "$REGION" \
    --worker-threads "$WORKERS" \
    --duration "$DURATION" \
    --segment-size "$SEGMENT_SIZE" \
    --total-size "$TOTAL_SIZE"

echo ""
echo "Checking S3 for uploaded objects..."
OBJECT_COUNT=$(aws s3api list-objects-v2 \
    --bucket "$BUCKET" \
    --prefix "$PREFIX" \
    --region "$REGION" \
    --query 'KeyCount' \
    --output text 2>/dev/null || echo "0")

echo "Objects in S3: $OBJECT_COUNT"

# Check for leftover sealed segments (not uploaded)
LEFTOVER=$(find "$TRACE_DIR" -name '*.bin' ! -name '*.active' 2>/dev/null | wc -l)
echo "Leftover sealed segments on disk: $LEFTOVER"

if [ "$LEFTOVER" -gt 0 ]; then
    echo ""
    echo "⚠ Worker could not keep up — $LEFTOVER segments remain on disk"
    ls -la "$TRACE_DIR"/*.bin 2>/dev/null || true
else
    echo ""
    echo "✓ Worker kept up — all segments uploaded"
fi
