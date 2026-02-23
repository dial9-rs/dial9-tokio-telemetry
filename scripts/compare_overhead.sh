#!/usr/bin/env bash
set -e

DURATION=${1:-30}
TELEMETRY_OVERHEAD_MAX_PCT=10
TRACE_BYTES_PER_REQUEST_MIN=20
TRACE_BYTES_PER_REQUEST_MAX=35
BYTES_PER_TRACE_EVENT_MIN=6
BYTES_PER_TRACE_EVENT_MAX=16

echo "Running overhead comparison (${DURATION}s per mode)..."
echo

# Build benchmark
echo "Building benchmark..."
cargo build --release --bench overhead_bench 2>&1 | grep -E "(Compiling|Finished)" || true

# Find the benchmark binary
BENCH_BIN=$(find ../.. -path "*/release/deps/overhead_bench-*" -type f -executable 2>/dev/null | head -1)

if [ -z "$BENCH_BIN" ]; then
    echo "Error: Could not find overhead_bench binary"
    exit 1
fi

echo "Using: $BENCH_BIN"
echo

# Run baseline
echo "=== Baseline (no telemetry) ==="
$BENCH_BIN baseline $DURATION --json > /tmp/baseline.json
cat /tmp/baseline.json

# Run telemetry
echo
echo "=== Telemetry enabled ==="
$BENCH_BIN telemetry $DURATION --json > /tmp/telemetry.json
cat /tmp/telemetry.json

# Parse JSON results
baseline_rps=$(grep '"throughput_rps"' /tmp/baseline.json | awk '{print $2}' | tr -d ',')
baseline_p50_ns=$(grep '"p50_lat_ns"' /tmp/baseline.json | awk '{print $2}' | tr -d ',')
baseline_p99_ns=$(grep '"p99_lat_ns"' /tmp/baseline.json | awk '{print $2}' | tr -d ',')

telemetry_rps=$(grep '"throughput_rps"' /tmp/telemetry.json | awk '{print $2}' | tr -d ',')
telemetry_p50_ns=$(grep '"p50_lat_ns"' /tmp/telemetry.json | awk '{print $2}' | tr -d ',')
telemetry_p99_ns=$(grep '"p99_lat_ns"' /tmp/telemetry.json | awk '{print $2}' | tr -d ',')
telemetry_requests=$(grep '"requests"' /tmp/telemetry.json | awk '{print $2}' | tr -d ',')

# Convert ns to µs for display
baseline_p50_us=$(awk "BEGIN {printf \"%.1f\", $baseline_p50_ns / 1000}")
baseline_p99_us=$(awk "BEGIN {printf \"%.1f\", $baseline_p99_ns / 1000}")
telemetry_p50_us=$(awk "BEGIN {printf \"%.1f\", $telemetry_p50_ns / 1000}")
telemetry_p99_us=$(awk "BEGIN {printf \"%.1f\", $telemetry_p99_ns / 1000}")

# Compute overhead
overhead_pct=$(awk "BEGIN {printf \"%.1f\", (($baseline_rps - $telemetry_rps) / $baseline_rps) * 100}")

# Trace file stats
trace_file="/tmp/overhead_bench_trace.bin"
if [ -f "$trace_file" ]; then
    trace_size=$(stat -c%s "$trace_file" 2>/dev/null || stat -f%z "$trace_file" 2>/dev/null)
    header_size=12
    event_bytes=$((trace_size - header_size))
    
    # Trace bytes per request (how much trace data per client request)
    trace_bytes_per_request=$(awk "BEGIN {printf \"%.2f\", $event_bytes / $telemetry_requests}")
    
    # Count actual trace events (estimate: 4 events per request = 2 polls + park + unpark)
    # Plus periodic queue samples (~100/sec)
    estimated_trace_events=$((telemetry_requests * 4 + DURATION * 100))
    bytes_per_trace_event=$(awk "BEGIN {printf \"%.2f\", $event_bytes / $estimated_trace_events}")
    
    events_per_sec=$(awk "BEGIN {printf \"%.0f\", $telemetry_requests / $DURATION}")
else
    trace_bytes_per_request="N/A"
    bytes_per_trace_event="N/A"
    events_per_sec="N/A"
fi

echo
echo "=== Comparison ==="
echo "Baseline:   ${baseline_rps} req/s, p50=${baseline_p50_us}µs, p99=${baseline_p99_us}µs"
echo "Telemetry:  ${telemetry_rps} req/s, p50=${telemetry_p50_us}µs, p99=${telemetry_p99_us}µs"
echo "Overhead:   ${overhead_pct}%"
echo
echo "=== Trace Efficiency ==="
echo "Trace bytes/request:    ${trace_bytes_per_request}"
echo "Bytes/trace event:      ${bytes_per_trace_event}"
echo "Client requests/sec:    ${events_per_sec}"

# Validation
echo
echo "=== Validation ==="
fail=0

# Check overhead
overhead_check=$(awk "BEGIN {print ($overhead_pct > $TELEMETRY_OVERHEAD_MAX_PCT)}")
if [ "$overhead_check" = "1" ]; then
    echo "❌ FAIL: Telemetry overhead too high (${overhead_pct}% > ${TELEMETRY_OVERHEAD_MAX_PCT}%)"
    fail=1
else
    echo "✅ PASS: Telemetry overhead acceptable (${overhead_pct}%)"
fi

# Check trace bytes per request
if [ "$trace_bytes_per_request" != "N/A" ]; then
    check=$(awk "BEGIN {print ($trace_bytes_per_request < $TRACE_BYTES_PER_REQUEST_MIN || $trace_bytes_per_request > $TRACE_BYTES_PER_REQUEST_MAX)}")
    if [ "$check" = "1" ]; then
        echo "❌ FAIL: Trace bytes/request out of range (${trace_bytes_per_request}, expected ${TRACE_BYTES_PER_REQUEST_MIN}-${TRACE_BYTES_PER_REQUEST_MAX})"
        fail=1
    else
        echo "✅ PASS: Trace bytes/request in expected range (${trace_bytes_per_request})"
    fi
fi

# Check bytes per trace event
if [ "$bytes_per_trace_event" != "N/A" ]; then
    check=$(awk "BEGIN {print ($bytes_per_trace_event < $BYTES_PER_TRACE_EVENT_MIN || $bytes_per_trace_event > $BYTES_PER_TRACE_EVENT_MAX)}")
    if [ "$check" = "1" ]; then
        echo "❌ FAIL: Bytes/trace event out of range (${bytes_per_trace_event}, expected ${BYTES_PER_TRACE_EVENT_MIN}-${BYTES_PER_TRACE_EVENT_MAX})"
        fail=1
    else
        echo "✅ PASS: Bytes/trace event in expected range (${bytes_per_trace_event})"
    fi
fi

echo
if [ $fail -eq 0 ]; then
    echo "🎉 All checks passed!"
    exit 0
else
    echo "💥 Some checks failed"
    exit 1
fi
