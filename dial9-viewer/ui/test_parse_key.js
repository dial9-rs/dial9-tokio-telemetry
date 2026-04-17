#!/usr/bin/env node
// Verify parseKey() in index.html understands both the new boot_id layout
// and the legacy (pre-#225) layout.

"use strict";
const fs = require("fs");
const path = require("path");
const vm = require("vm");

const html = fs.readFileSync(path.join(__dirname, "index.html"), "utf8");
const m = html.match(/function parseKey\([\s\S]*?\n    \}\n/);
if (!m) {
    console.error("could not locate parseKey() in index.html");
    process.exit(1);
}

// parseKey depends on formatEpoch/formatDate — stub them out.
const sandbox = { parseKey: null, formatEpoch: () => "" };
vm.createContext(sandbox);
vm.runInContext(m[0] + "\nthis.parseKey = parseKey;", sandbox);
const parseKey = sandbox.parseKey;

function assertEq(actual, expected, label) {
    if (actual !== expected) {
        console.error(
            `✗ ${label}: expected ${JSON.stringify(expected)}, got ${JSON.stringify(actual)}`,
        );
        process.exit(1);
    } else {
        console.log(`✓ ${label}`);
    }
}

// New layout with prefix
let p = parseKey(
    "traces/2026-04-09/1910/checkout-api/us-east-1/abcd-123213/1744224000-3.bin.gz",
);
assertEq(p.service, "checkout-api", "new layout: service");
assertEq(p.host, "us-east-1", "new layout: host");
assertEq(p.bootId, "abcd-123213", "new layout: bootId");
assertEq(p.epoch, 1744224000, "new layout: epoch");
assertEq(p.segIndex, "3", "new layout: segIndex");

// New layout without prefix
p = parseKey(
    "2026-04-09/1910/checkout-api/us-east-1/xyzw-asdfasdf/1744224000-0.bin.gz",
);
assertEq(p.service, "checkout-api", "new no-prefix: service");
assertEq(p.host, "us-east-1", "new no-prefix: host");
assertEq(p.bootId, "xyzw-asdfasdf", "new no-prefix: bootId");

// Legacy layout with prefix — unchanged behavior
p = parseKey("traces/2026-04-09/1910/checkout-api/host1/1744224000-2.bin.gz");
assertEq(p.service, "checkout-api", "legacy: service");
assertEq(p.host, "host1", "legacy: host");
assertEq(p.bootId, "", "legacy: bootId empty");
assertEq(p.epoch, 1744224000, "legacy: epoch");
assertEq(p.segIndex, "2", "legacy: segIndex");

// Instance path with embedded slash is a best-effort legacy case —
// cannot be reliably distinguished from the new boot_id layout on
// path-component count alone, so the parser falls back to the positional
// heuristic. We just sanity-check that parsing does not throw.
p = parseKey(
    "traces/2026-04-09/1910/checkout-api/us-east-1/i-0abc123/1744224000-0.bin.gz",
);
if (!p || typeof p !== "object") {
    console.error("compound-instance: must return object");
    process.exit(1);
}
console.log("✓ compound-instance: returns object (best-effort)");

console.log("\nAll parseKey tests passed");
