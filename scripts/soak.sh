#!/usr/bin/env bash
# Runs the engine soak with every round in a fresh process (a restart, as in practice): recover
# the journal and snapshot, place a million orders over gRPC with cancels, report memory.
# Usage: scripts/soak.sh [rounds] [orders per round]
set -euo pipefail
rounds="${1:-4}"
orders="${2:-1000000}"
dir="$(mktemp -d)"
trap 'rm -rf "$dir"' EXIT
cargo build -q --release -p engine-server --example soak
for r in $(seq 1 "$rounds"); do
  SOAK_JOURNAL="$dir/journal.jsonl" SOAK_FIRST_ROUND="$r" SOAK_ROUNDS=1 SOAK_ORDERS="$orders" \
    ./target/release/examples/soak | grep '^round'
done
