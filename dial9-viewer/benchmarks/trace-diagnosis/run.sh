#!/usr/bin/env bash
set -euo pipefail

SCRIPT_DIR="$(cd "$(dirname "$0")" && pwd)"
TARGET="/tmp/dial9-bench-target"
CLEAN=""
MODEL=""
AGENT=""

usage() {
    echo "Usage: ./run.sh [target-dir] [--clean] [--model <model>] [--agent <agent>]"
    echo ""
    echo "  target-dir    Path to the test project (default: /tmp/dial9-bench-target)"
    echo "  --clean       Regenerate the target project from scratch"
    echo "  --model       Model to use (e.g. claude-sonnet-4-20250514)"
    echo "  --agent       Agent to use (e.g. kiro, claude, codex)"
    exit 1
}

while [[ $# -gt 0 ]]; do
    case "$1" in
        --clean) CLEAN="yes"; shift ;;
        --model) MODEL="$2"; shift 2 ;;
        --agent) AGENT="$2"; shift 2 ;;
        --help|-h) usage ;;
        -*) usage ;;
        *) TARGET="$1"; shift ;;
    esac
done

# Setup if target doesn't exist or --clean
if [[ ! -d "$TARGET/${SKILLS_DIR:-.claude/skills}" ]] || [[ -n "$CLEAN" ]]; then
    "$SCRIPT_DIR/setup.sh" "$TARGET"
fi

TRACE="$TARGET/trace.bin"
LOG="/tmp/dial9-skill-benchmark-$(date +%Y%m%d-%H%M%S)"

PROMPT="Analyze the dial9 trace at ./trace.bin using the toolkit in .d9-toolkit/. Identify performance problems, their root causes, and recommend fixes. Run the red flag scan first (node .d9-toolkit/analyze.js ./trace.bin), then drill into the worst findings with specific recipes from the dial9 skills. Show your work: include the commands you run and the key numbers."

echo ""
echo "Running benchmark..."
echo "Log: $LOG.md"
echo "Start: $(date -Iseconds)"
echo "---"
echo ""

START_TIME=$(date +%s)

MODEL_FLAG=""
if [[ -n "$MODEL" ]]; then
    MODEL_FLAG="--model $MODEL"
fi

cd "$TARGET"
case "${AGENT:-claude}" in
    claude)
        echo "$PROMPT" | claude -p --verbose --output-format stream-json \
            $MODEL_FLAG \
            --allowed-tools "Read,Glob,Grep,Skill,Bash(node *)" \
            | tee "$LOG.raw" \
            | jq -r --unbuffered 'select(.type == "assistant") | .message.content[]? | select(.type == "text" or .type == "thinking") | if .type == "thinking" then "<thinking>\n\(.thinking)\n</thinking>" else .text // empty end' \
            | tee "$LOG.md"
        ;;
    kiro)
        echo "$PROMPT" | kiro chat --no-interactive $MODEL_FLAG 2>&1 | tee "$LOG.md"
        ;;
    codex)
        echo "$PROMPT" | codex --quiet $MODEL_FLAG 2>&1 | tee "$LOG.md"
        ;;
    *)
        echo "Unknown agent: $AGENT (supported: claude, kiro, codex)"
        exit 1
        ;;
esac

END_TIME=$(date +%s)
ELAPSED=$((END_TIME - START_TIME))

# Write run summary
cat > "$LOG.summary" <<EOF
start: $(date -Iseconds -d @$START_TIME 2>/dev/null || date -r $START_TIME +%Y-%m-%dT%H:%M:%S%z)
end: $(date -Iseconds -d @$END_TIME 2>/dev/null || date -r $END_TIME +%Y-%m-%dT%H:%M:%S%z)
duration: ${ELAPSED}s
model: ${MODEL:-default}
agent: ${AGENT:-claude}
target: $TARGET
EOF

echo ""
echo "---"
echo "End: $(date -Iseconds)"
echo "Duration: ${ELAPSED}s"
echo "Output: $LOG.md"
echo "Raw JSON: $LOG.raw"
echo "Summary: $LOG.summary"

# Extract agent tool usage log (claude only, requires raw JSON)
if [[ -f "$LOG.raw" ]]; then
    jq -r 'select(.type == "assistant") | .message.content[]? | select(.type == "tool_use") | "\(.name): \(.input | tostring | .[0:200])"' "$LOG.raw" > "$LOG.commands"
    jq -r 'select(.type == "assistant") | .message.content[]? | select(.type == "thinking") | .thinking' "$LOG.raw" > "$LOG.thinking"
    echo "Commands: $LOG.commands"
    echo "Thinking: $LOG.thinking"
fi
echo ""
echo "Evaluate with:"
echo "  Evaluate $LOG.md against $SCRIPT_DIR/EXPECTED.md"
