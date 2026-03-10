#!/usr/bin/env bash
# Integration test: runs the metrics-service with S3 upload enabled,
# then periodically checks that trace segments land in S3 and can be
# analyzed.
set -euo pipefail

REPO_ROOT="$(git -C "$(dirname "$0")" rev-parse --show-toplevel)"
cd "$REPO_ROOT"

BUCKET="dial9-integ-test-$$-$(date +%s)"
REGION="${AWS_REGION:-us-east-1}"
TRACE_PATH="/tmp/dial9-integ-test-$$/trace.bin"
SERVICE_PID=""
POLL_INTERVAL=5      # seconds between S3 checks
MAX_WAIT=120         # total seconds before giving up
RUN_DURATION=60      # how long the service runs

cleanup() {
    echo "--- cleanup ---"
    if [ -n "$SERVICE_PID" ] && kill -0 "$SERVICE_PID" 2>/dev/null; then
        echo "Stopping metrics-service (pid $SERVICE_PID)..."
        kill "$SERVICE_PID" || true
        wait "$SERVICE_PID" 2>/dev/null || true
    fi

    echo "Deleting S3 objects..."
    aws s3 rm "s3://$BUCKET" --recursive --region "$REGION" 2>/dev/null || true
    echo "Deleting S3 bucket..."
    aws s3api delete-bucket --bucket "$BUCKET" --region "$REGION" 2>/dev/null || true

    rm -rf "/tmp/dial9-integ-test-$$"
    echo "Cleanup done."
}
trap cleanup EXIT

# --- build ---
echo "Building metrics-service (release)..."
cargo build --release -p metrics-service

# --- create bucket ---
echo "Creating S3 bucket: $BUCKET (region: $REGION)"
if [ "$REGION" = "us-east-1" ]; then
    aws s3api create-bucket --bucket "$BUCKET" --region "$REGION"
else
    aws s3api create-bucket --bucket "$BUCKET" --region "$REGION" \
        --create-bucket-configuration LocationConstraint="$REGION"
fi

# --- add lifecycle rule to auto-expire objects after 1 day (safety net) ---
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

# --- start service ---
mkdir -p "$(dirname "$TRACE_PATH")"
echo "Starting metrics-service with --s3-bucket $BUCKET ..."
AWS_PROFILE="${AWS_PROFILE:-}" cargo run --release -p metrics-service --bin metrics-service -- \
    --trace-path "$TRACE_PATH" \
    --s3-bucket "$BUCKET" \
    --run-duration "$RUN_DURATION" \
    --trace-max-file-size 524288 \
    --trace-max-total-size 2097152 &
SERVICE_PID=$!
echo "Service PID: $SERVICE_PID"

# --- poll S3 for uploaded traces ---
echo "Polling S3 for trace uploads (every ${POLL_INTERVAL}s, timeout ${MAX_WAIT}s)..."
ELAPSED=0
OBJECTS_FOUND=0
DOWNLOAD_DIR="/tmp/dial9-integ-test-$$/downloaded"
mkdir -p "$DOWNLOAD_DIR"

ANALYZE_BIN="$REPO_ROOT/target/release/examples/analyze_trace"
if [ ! -f "$ANALYZE_BIN" ]; then
    echo "Building analyze_trace example..."
    cargo build --release -p dial9-tokio-telemetry --example analyze_trace
fi

while [ "$ELAPSED" -lt "$MAX_WAIT" ]; do
    sleep "$POLL_INTERVAL"
    ELAPSED=$((ELAPSED + POLL_INTERVAL))

    # List objects
    KEYS=$(aws s3api list-objects-v2 \
        --bucket "$BUCKET" \
        --region "$REGION" \
        --query 'Contents[].Key' \
        --output text 2>/dev/null || echo "")

    if [ -z "$KEYS" ] || [ "$KEYS" = "None" ]; then
        echo "  [${ELAPSED}s] No objects yet..."
        continue
    fi

    NEW_COUNT=$(echo "$KEYS" | wc -w)
    echo "  [${ELAPSED}s] Found $NEW_COUNT object(s) in S3"

    # Download and analyze any new objects
    for KEY in $KEYS; do
        SAFE_NAME=$(echo "$KEY" | tr '/' '_')
        LOCAL_GZ="$DOWNLOAD_DIR/$SAFE_NAME"
        LOCAL_BIN="${LOCAL_GZ%.gz}"

        # Skip already-processed files
        if [ -f "$LOCAL_BIN" ]; then
            continue
        fi

        echo "  Downloading: $KEY"
        aws s3api get-object \
            --bucket "$BUCKET" \
            --key "$KEY" \
            --region "$REGION" \
            "$LOCAL_GZ" > /dev/null

        echo "  Decompressing..."
        gunzip -f "$LOCAL_GZ"

        echo "  Analyzing trace: $LOCAL_BIN"
        if "$ANALYZE_BIN" "$LOCAL_BIN"; then
            echo "  ✓ Analysis succeeded for $KEY"
            OBJECTS_FOUND=$((OBJECTS_FOUND + 1))
        else
            echo "  ✗ Analysis FAILED for $KEY" >&2
            exit 1
        fi
    done

    # Once we've successfully analyzed at least one object, we're good
    if [ "$OBJECTS_FOUND" -ge 1 ]; then
        echo ""
        echo "✓ Integration test passed: $OBJECTS_FOUND trace segment(s) uploaded, downloaded, and analyzed."
        exit 0
    fi
done

echo "✗ Timed out after ${MAX_WAIT}s waiting for trace uploads" >&2
exit 1
