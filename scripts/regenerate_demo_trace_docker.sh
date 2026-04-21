#!/usr/bin/env bash
set -euo pipefail

REPO_ROOT="$(git -C "$(dirname "$0")" rev-parse --show-toplevel)"
cd "$REPO_ROOT"

trap 'docker compose down --remove-orphans >/dev/null 2>&1 || true' EXIT

docker compose run --rm --build regenerate
