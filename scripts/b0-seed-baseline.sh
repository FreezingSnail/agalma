#!/bin/sh
# B0.6 resource gate — reproducible seed-executor baseline measurement.
#
# Requirement: docs/durable-execution-bootstrap.md §"Acceptance and evaluation"
# (compare resource measurements with the seed executor; establish numeric
# regression limits from the measured seed; correctness is a veto).
#
# This wrapper runs the co-located Rust measurement harness
# `crates/agalma-kernel/tests/resources.rs` and prints where the JSON report
# landed. All run artifacts live under `target/test-runs/b0-resources/`
# (never /tmp). Numbers feed `docs/b0-exit-evidence.md`.
#
# Usage:  sh scripts/b0-seed-baseline.sh

set -eu

cd "$(dirname "$0")/.."

cargo test -p agalma-kernel --offline --test resources -- --nocapture

echo
echo "report: target/test-runs/b0-resources/seed-baseline.json"
