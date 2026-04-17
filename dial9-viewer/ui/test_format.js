#!/usr/bin/env node
"use strict";

// Unit tests for formatHumanDuration.
//
// The function takes a nanosecond value and returns a human-friendly string
// that picks a sensible unit (ns, µs, ms, s, m, h, d). This fixes the case
// where long traces show durations like "28808404.3ms" (8 hours) in the UI.

const { formatHumanDuration } = require("./format.js");

let failed = 0;
let passed = 0;

function assertEq(actual, expected, desc) {
  if (actual === expected) {
    console.log(`✓ ${desc}`);
    passed++;
  } else {
    console.log(`✗ ${desc}\n    expected: ${JSON.stringify(expected)}\n    actual:   ${JSON.stringify(actual)}`);
    failed++;
  }
}

// Sub-microsecond → ns
assertEq(formatHumanDuration(0), "0ns", "zero");
assertEq(formatHumanDuration(500), "500ns", "500 ns");
assertEq(formatHumanDuration(999), "999ns", "999 ns");

// Microseconds
assertEq(formatHumanDuration(1_000), "1.0µs", "1 µs");
assertEq(formatHumanDuration(1_500), "1.5µs", "1.5 µs");
assertEq(formatHumanDuration(999_999), "1000.0µs", "just under 1 ms");

// Milliseconds
assertEq(formatHumanDuration(1_000_000), "1.00ms", "1 ms");
assertEq(formatHumanDuration(123_456_789), "123.46ms", "123 ms");
assertEq(formatHumanDuration(999_000_000), "999.00ms", "999 ms");

// Seconds
assertEq(formatHumanDuration(1_000_000_000), "1.00s", "1 s");
assertEq(formatHumanDuration(59_000_000_000), "59.00s", "59 s");

// Minutes (>= 60s)
assertEq(formatHumanDuration(60_000_000_000), "1m 0.0s", "60 s → 1m 0.0s");
assertEq(formatHumanDuration(90_000_000_000), "1m 30.0s", "90 s → 1m 30s");
assertEq(formatHumanDuration(3_599_000_000_000), "59m 59.0s", "just under 1 hour");

// Hours (>= 60 minutes)
assertEq(formatHumanDuration(3_600_000_000_000), "1h 0m 0s", "1 hour");
// The bug report case: 28,808,404.3 ms ≈ 8h 0m 8s
assertEq(formatHumanDuration(28_808_404_300_000), "8h 0m 8s", "8-hour trace from issue #200");

// Days (>= 24 hours)
assertEq(formatHumanDuration(86_400_000_000_000), "1d 0h 0m", "1 day");
assertEq(formatHumanDuration(90_000_000_000_000), "1d 1h 0m", "1d 1h");

// ── Summary ──
console.log(`\n${passed} passed, ${failed} failed`);
process.exit(failed === 0 ? 0 : 1);
