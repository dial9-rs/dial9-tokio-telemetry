#!/usr/bin/env bash
# Validate JavaScript syntax in trace_viewer.html
set -euo pipefail

SCRIPT_DIR="$(cd "$(dirname "$0")" && pwd)"
HTML="$SCRIPT_DIR/../trace_viewer.html"
TMP=$(mktemp /tmp/viewer_check_XXXXXX.js)
trap 'rm -f "$TMP"' EXIT

sed -n '/<script>/,/<\/script>/p' "$HTML" | sed '1d;$d' > "$TMP"
if node --check "$TMP" 2>&1; then
    echo "✓ trace_viewer.html: JS syntax OK"
else
    exit 1
fi
