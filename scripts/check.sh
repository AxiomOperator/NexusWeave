#!/usr/bin/env sh
# Pre-commit gate: the same checks CI runs (lint + tests).
set -eu
cd "$(dirname "$0")/.."
cargo clippy --release --all-targets -q -- -D warnings
cargo test --release -q
echo "check: ok"
