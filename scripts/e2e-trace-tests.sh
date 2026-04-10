#!/usr/bin/env bash
#
# e2e-trace-tests.sh -- End-to-end trace pipeline test
#
# Regenerates the demo trace against a local DynamoDB, then runs the
# JS-based trace integrity and analysis test suites.
#
#
# Usage:
#   scripts/e2e-trace-tests.sh                  # default (DDB on :8000)
#   scripts/e2e-trace-tests.sh --ddb-port=4566  # custom DDB port
#
# Options:
#   --ddb-port PORT  Port where DynamoDB Local is listening (default: 8000).
set -euo pipefail

DDB_PORT=8000
while [[ $# -gt 0 ]]; do
    case "$1" in
        --ddb-port=*) DDB_PORT="${1#*=}"; shift ;;
        --ddb-port)   DDB_PORT="$2"; shift 2 ;;
        *) echo "Unknown option: $1" >&2; exit 1 ;;
    esac
done

REPO_ROOT="$(git -C "$(dirname "$0")" rev-parse --show-toplevel)"
cd "$REPO_ROOT"

export AWS_ENDPOINT_URL="${AWS_ENDPOINT_URL:-http://localhost:$DDB_PORT}"
export AWS_ACCESS_KEY_ID="${AWS_ACCESS_KEY_ID:-local}"
export AWS_SECRET_ACCESS_KEY="${AWS_SECRET_ACCESS_KEY:-local}"
export AWS_DEFAULT_REGION="${AWS_DEFAULT_REGION:-us-east-1}"
export AWS_PROFILE="${AWS_PROFILE:-fake-profile}"

echo "--- Regenerating demo trace ---"
scripts/regenerate_demo_trace.sh

echo "--- Checking trace integrity ---"
node dial9-viewer/ui/test_trace_integrity.js

echo "--- Checking trace analysis ---"
node dial9-viewer/ui/test_trace_analysis.js

echo "All E2E trace checks passed."
