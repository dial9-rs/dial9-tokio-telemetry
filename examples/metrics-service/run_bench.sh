#!/usr/bin/env bash
set -euo pipefail

cd "$(dirname "$0")"
RESULTS_DIR="./bench-results"
mkdir -p "$RESULTS_DIR"

echo "=== Building release ==="
cargo build --release -p metrics-service 2>&1 | tail -3

BIN="$(cargo metadata --format-version 1 --no-deps 2>/dev/null | python3 -c 'import sys,json; print(json.load(sys.stdin)["target_directory"])')/release/metrics-service"

run_scenario() {
    local label="$1"
    shift
    local outfile="$RESULTS_DIR/${label}.csv"
    echo "=== Running: $label ==="
    env "$@" "$BIN" --run-duration 20 --worker-threads 4 2>&1 | \
        grep '^TIMESERIES:' | sed 's/^TIMESERIES://' > "$outfile"
    echo "  -> $(wc -l < "$outfile") data points in $outfile"
}

run_scenario "without_alloc_fd" RUST_LOG=warn
run_scenario "with_alloc_fd" RUST_LOG=warn PREWARM_FD_TABLE_SIZE=65536

echo "=== Generating graphs ==="
python3 "$RESULTS_DIR/../plot_bench.py" "$RESULTS_DIR"
echo "Done! Graphs in $RESULTS_DIR/"
