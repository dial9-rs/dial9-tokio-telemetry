#!/bin/bash
set -euo pipefail

# Regenerates dial9-viewer/skills/setup.md from the dial9-tokio-telemetry README.
#
# Usage:
#   ./scripts/sync-setup-skill.sh          # Regenerate in place
#   ./scripts/sync-setup-skill.sh --check  # Check for staleness (CI mode)

REPO_ROOT="$(cd "$(dirname "$0")/.." && pwd)"
README="$REPO_ROOT/dial9-tokio-telemetry/README.md"
DEST="$REPO_ROOT/dial9-viewer/skills/setup.md"

if [ ! -f "$README" ]; then
  echo "ERROR: $README not found. Run from the repo root." >&2
  exit 1
fi

SECTIONS=(
  "Prerequisites"
  "Quick start"
  "The root future is not instrumented"
  "Tracing span events (opt-in)"
  "Wake event tracking"
)

extract_section() {
  local file="$1" heading="$2"
  awk -v h="## $heading" '
    $0 == h { found=1 }
    found && /^## / && $0 != h { exit }
    found { print }
  ' "$file"
}

generate() {
  echo "# Instrumenting your app with dial9"
  echo ""
  echo "*This content is extracted from the [dial9-tokio-telemetry README](https://github.com/dial9-rs/dial9-tokio-telemetry).*"
  echo ""
  for section in "${SECTIONS[@]}"; do
    content="$(extract_section "$README" "$section")"
    if [ -z "$content" ]; then
      echo "ERROR: section '## $section' not found in README" >&2
      exit 1
    fi
    echo "$content"
  done
}

generated="$(generate)"

if [ "${1:-}" = "--check" ]; then
  if [ ! -f "$DEST" ]; then
    echo "ERROR: $DEST does not exist. Run: ./scripts/sync-setup-skill.sh" >&2
    exit 1
  fi
  if ! diff -q <(echo "$generated") "$DEST" > /dev/null 2>&1; then
    echo "ERROR: $DEST is stale. Run: ./scripts/sync-setup-skill.sh" >&2
    diff --unified <(echo "$generated") "$DEST" || true
    exit 1
  fi
  echo "OK: $DEST is up to date"
else
  echo "$generated" > "$DEST"
  echo "Wrote $DEST"
fi
