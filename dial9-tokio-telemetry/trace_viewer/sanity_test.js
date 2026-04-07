#!/usr/bin/env node
"use strict";

const fs = require("fs");
const path = require("path");

const error = (message) => {
  console.error(message);
  process.exit(1);
};

const tracePath = process.argv[2] || path.join(__dirname, "demo-trace.bin");

if (!fs.existsSync(tracePath)) {
  error(`Trace file not found: ${tracePath}`);
}

const stat = fs.statSync(tracePath);
console.log(`Found trace: ${tracePath} (${stat.size} bytes)`);

if (stat.size === 0) {
  error("Trace file is empty");
}

console.log("Sanity check passed");
