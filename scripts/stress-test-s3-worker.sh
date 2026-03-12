#!/usr/bin/env bash
# Stress test: can the S3 background worker keep up with a 64-worker runtime
# producing trace segments at high throughput?
#
# Usage:
#   ./scripts/stress-test-s3-worker.sh
#
# Optional env vars:
#   S3_STRESS_BUCKET — use an existing bucket (skips create/delete)
#   AWS_REGION       — defaults to us-east-1
#   WORKER_THREADS   — defaults to 64
#   RUN_DURATION     — seconds to run, defaults to 60
#   SEGMENT_SIZE     — bytes per segment before rotation, defaults to 262144 (256KB)
#   TOTAL_SIZE       — max total disk, defaults to 10MB
set -euo pipefail

BUCKET="${S3_STRESS_BUCKET:-dial9-stress-test-$$-$(date +%s)}"
REGION="${AWS_REGION:-us-east-1}"
WORKERS="${WORKER_THREADS:-64}"
DURATION="${RUN_DURATION:-60}"
SEGMENT_SIZE="${SEGMENT_SIZE:-20971520}"
TOTAL_SIZE="${TOTAL_SIZE:-104857600}"
MANAGE_BUCKET="${S3_STRESS_BUCKET:+no}"  # if user provided bucket, don't manage it
MANAGE_BUCKET="${MANAGE_BUCKET:-yes}"

REPO_ROOT="$(git -C "$(dirname "$0")" rev-parse --show-toplevel)"
cd "$REPO_ROOT"

TRACE_DIR=$(mktemp -d /tmp/dial9-stress-XXXXXX)
TRACE_PATH="$TRACE_DIR/trace.bin"
PREFIX="stress-test/$(date -u +%Y-%m-%dT%H%M%S)"

cleanup() {
    echo "--- cleanup ---"
    if [ "$MANAGE_BUCKET" = "yes" ]; then
        echo "Deleting S3 objects..."
        aws s3 rm "s3://$BUCKET" --recursive --region "$REGION" 2>/dev/null || true
        echo "Deleting S3 bucket..."
        aws s3api delete-bucket --bucket "$BUCKET" --region "$REGION" 2>/dev/null || true
    fi
    rm -rf "$TRACE_DIR"
    echo "Cleanup done."
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

# --- create bucket if needed ---
if [ "$MANAGE_BUCKET" = "yes" ]; then
    echo "Creating S3 bucket: $BUCKET (region: $REGION)"
    if [ "$REGION" = "us-east-1" ]; then
        aws s3api create-bucket --bucket "$BUCKET" --region "$REGION"
    else
        aws s3api create-bucket --bucket "$BUCKET" --region "$REGION" \
            --create-bucket-configuration LocationConstraint="$REGION"
    fi

    echo "Adding 1-day expiration lifecycle rule..."
    aws s3api put-bucket-lifecycle-configuration \
        --bucket "$BUCKET" \
        --region "$REGION" \
        --lifecycle-configuration '{
            "Rules": [{
                "ID": "expire-all-1d",
                "Status": "Enabled",
                "Filter": {},
                "Expiration": { "Days": 1 }
            }]
        }'
fi

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
