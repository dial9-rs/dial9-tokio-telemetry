#!/usr/bin/env bash
set -euo pipefail

SCRIPT_DIR="$(cd "$(dirname "$0")" && pwd)"
TARGET="/tmp/dial9-bench-target"
CLEAN=""

usage() {
    echo "Usage: ./run.sh [target-dir] [--clean]"
    echo ""
    echo "  target-dir    Path to the test project (default: /tmp/dial9-bench-target)"
    echo "  --clean       Regenerate the target project from scratch"
    exit 1
}

while [[ $# -gt 0 ]]; do
    case "$1" in
        --clean) CLEAN="yes"; shift ;;
        --help|-h) usage ;;
        -*) usage ;;
        *) TARGET="$1"; shift ;;
    esac
done

# Setup if target doesn't exist or --clean
if [[ ! -d "$TARGET/.kiro/skills" ]] || [[ -n "$CLEAN" ]]; then
    "$SCRIPT_DIR/setup.sh" "$TARGET"
fi

TRACE="$TARGET/trace.bin"
LOG="/tmp/dial9-skill-benchmark-$(date +%Y%m%d-%H%M%S)"

PROMPT="Analyze the dial9 trace at ./trace.bin using the toolkit in .d9-toolkit/. Identify performance problems, their root causes, and recommend fixes. Run the red flag scan first (node .d9-toolkit/analyze.js ./trace.bin), then drill into the worst findings with specific recipes from the dial9 skills. Show your work: include the commands you run and the key numbers."

echo ""
echo "Running benchmark..."
echo "Log: $LOG.md"
echo "---"
echo ""

cd "$TARGET"
echo "$PROMPT" | claude -p --verbose --output-format stream-json \
    --allowed-tools "Read,Glob,Grep,Skill,Bash(node *)" \
    | tee "$LOG.raw" \
    | jq -r --unbuffered 'select(.type == "assistant") | .message.content[]? | select(.type == "text") | .text // empty' \
    | tee "$LOG.md"

echo ""
echo "---"
echo "Output: $LOG.md"
echo "Raw JSON: $LOG.raw"
echo ""
echo "Evaluate with:"
echo "  Evaluate $LOG.md against $SCRIPT_DIR/EXPECTED.md"
