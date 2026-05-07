#!/usr/bin/env bash
set -euo pipefail

SCRIPT_DIR="$(cd "$(dirname "$0")" && pwd)"
REPO_ROOT="$(cd "$SCRIPT_DIR/../.." && pwd)"
TARGET="${1:-/tmp/dial9-bench-target}"

echo "Creating target project at $TARGET..."
rm -rf "$TARGET"
mkdir -p "$TARGET/src"

# Minimal Cargo.toml that depends on dial9-tokio-telemetry
cat > "$TARGET/Cargo.toml" <<EOF
[package]
name = "dial9-bench-target"
version = "0.1.0"
edition = "2024"

[dependencies]
dial9-tokio-telemetry = { path = "$REPO_ROOT/../dial9-tokio-telemetry" }
tokio = { version = "1", features = ["full"] }
EOF

# Minimal main.rs
cat > "$TARGET/src/main.rs" <<'EOF'
#[tokio::main]
async fn main() {
    println!("bench target");
}
EOF

# Init git (agents need it)
cd "$TARGET"
git init -q
git add -A
git commit -q -m "init"

# Copy demo trace
cp "$REPO_ROOT/ui/demo-trace.bin" "$TARGET/trace.bin"

# Install skills directly into the target project
echo "Installing dial9 skills..."
VIEWER_BIN="${DIAL9_VIEWER_BIN:-$(command -v dial9-viewer 2>/dev/null || echo "")}"
if [[ -z "$VIEWER_BIN" ]]; then
    # Try building from source
    VIEWER_BIN="$(cd "$REPO_ROOT/.." && cargo build -p dial9-viewer --message-format short 2>/dev/null | grep "dial9-viewer$" || echo "")"
fi
if [[ -z "$VIEWER_BIN" ]]; then
    VIEWER_BIN="$REPO_ROOT/../target/debug/dial9-viewer"
fi

"$VIEWER_BIN" agents skills "$TARGET/.kiro/skills"

# Also extract toolkit flat for easy node access
"$VIEWER_BIN" agents toolkit "$TARGET/.d9-toolkit"

# Stage the installed skills
cd "$TARGET"
git add -A
git commit -q -m "add dial9 skills and toolkit"

echo ""
echo "Setup complete: $TARGET"
echo "Trace: $TARGET/trace.bin"
echo "Toolkit: $TARGET/.d9-toolkit/"
echo "Skills:"
ls "$TARGET/.kiro/skills/"
