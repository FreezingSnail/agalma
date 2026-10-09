#!/usr/bin/env bash
# S0c durability crash-matrix runner.
#
# Builds the std-only spike offline and runs all six cases. Each case writes a
# printed report and spikes/s0c/runs/<case>/evidence.txt. Exits non-zero if any
# case fails.
#
# Usage:
#   spikes/s0c/run_all.sh
#
# Equivalent single-case invocation:
#   cargo run --release --manifest-path spikes/s0c/Cargo.toml -- case1
#   (or: cd spikes/s0c && cargo run --release -- case1)

set -euo pipefail

here="$(cd "$(dirname "$0")" && pwd)"

echo "[s0c] building (offline, std-only)"
cargo build --release --offline --manifest-path "$here/Cargo.toml"

overall=0
for c in case1 case2 case3 case4 case5 case6; do
  echo
  if "$here/target/release/s0c" "$c"; then
    echo "[s0c] $c: OK"
  else
    echo "[s0c] $c: FAILED"
    overall=1
  fi
done

echo
if [ "$overall" -eq 0 ]; then
  echo "[s0c] RESULT: all cases PASS"
else
  echo "[s0c] RESULT: one or more cases FAILED"
fi
exit "$overall"
