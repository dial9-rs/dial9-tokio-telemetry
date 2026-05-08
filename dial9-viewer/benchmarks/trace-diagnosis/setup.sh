#!/usr/bin/env bash
set -euo pipefail

SCRIPT_DIR="$(cd "$(dirname "$0")" && pwd)"
REPO_ROOT="$(cd "$SCRIPT_DIR/../.." && pwd)"
TARGET="${1:-/tmp/dial9-bench-target}"

echo "Creating target project at $TARGET..."
rm -rf "$TARGET"

# Copy the metrics-service example as a realistic project
cp -r "$REPO_ROOT/../examples/metrics-service" "$TARGET"

# Init git (agents need it)
cd "$TARGET"
git init -q
git add -A
git commit -q -m "init"

# Copy demo trace
cp "$REPO_ROOT/ui/demo-trace.bin" "$TARGET/trace.bin"

# Install skills
echo "Installing dial9 skills..."
VIEWER_BIN="${DIAL9_VIEWER_BIN:-$(command -v dial9-viewer 2>/dev/null || echo "")}"
if [[ -z "$VIEWER_BIN" ]]; then
    VIEWER_BIN="$REPO_ROOT/../target/debug/dial9-viewer"
fi

"$VIEWER_BIN" agents skills "$TARGET/${SKILLS_DIR:-.claude/skills}"

# Extract toolkit
"$VIEWER_BIN" agents toolkit "$TARGET/.d9-toolkit"

# Stage everything
cd "$TARGET"
git add -A
git commit -q -m "add dial9 skills and toolkit"

echo ""
echo "Setup complete: $TARGET"
echo "Trace: $TARGET/trace.bin"
echo "Toolkit: $TARGET/.d9-toolkit/"
echo "Skills:"
ls "$TARGET/${SKILLS_DIR:-.claude/skills}/"
