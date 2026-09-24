#!/usr/bin/env bash
# WHERE THE BUILD OUTPUT IS: cargo's own answer (`target_directory`), never a path written down here. A literal
# `target/` ignored CARGO_TARGET_DIR, so every checkout grew its own multi-GB build cache beside the one its
# session named. Every script that reads a built file asks this, from the workspace root.
set -euo pipefail
cd "$(dirname "$0")/.."
cargo metadata --format-version 1 --no-deps |
  python3 -c 'import json,sys; print(json.load(sys.stdin)["target_directory"])'
