#!/usr/bin/env bash
set -euo pipefail

RUSTFLAGS="--cfg tokio_unstable --cfg shuttle" \
  cargo llvm-cov --lib --features _shuttle --html -p dial9-tokio-telemetry -- shuttle "$@"

echo "Report: target/llvm-cov/html/index.html"
open target/llvm-cov/html/index.html 2>/dev/null || true
