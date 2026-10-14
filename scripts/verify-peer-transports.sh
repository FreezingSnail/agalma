#!/usr/bin/env bash
# FEZ acceptance: real local processes, default Connect wire, no paid inference.
set -euo pipefail

workspace_dir="$(cd -- "$(dirname -- "${BASH_SOURCE[0]}")/.." && pwd)"
cd "$workspace_dir"
unset AGALMA_IPC_WIRE AGALMA_HARNESS_LIVE AGALMA_HARNESS_TRANSPORT
unset AGALMA_LEDGER_TRANSPORT AGALMA_WORKSPACE_TRANSPORT
unset AGALMA_TASKQUEUE_TRANSPORT AGALMA_EXECUTION_TRANSPORT

cargo build --offline --workspace --bins
target_dir="${CARGO_TARGET_DIR:-target}"
if [[ -n "${CARGO_BUILD_TARGET:-}" ]]; then
    target_dir="$target_dir/$CARGO_BUILD_TARGET"
fi

# macOS may assess a new executable before it reaches main. Exercise only the
# argument parser, without sockets or effects, before measuring service READY.
# Keep the production READY deadline unchanged and retain cold-launch failures
# in the evidence record; this is explicit build qualification, not a retry.
for component in ledger workspace taskqueue kernel sandbox; do
    binary="$target_dir/debug/agalma-$component-server"
    test -x "$binary"
    shasum -a 256 "$binary"
    if output="$("$binary" 2>&1)"; then
        printf 'Expected argument-parser rejection from %s\n' "$binary" >&2
        exit 1
    fi
    if [[ "$output" != *'missing required --socket <path>'* ]]; then
        printf 'Preflight failed for %s: %s\n' "$binary" "$output" >&2
        exit 1
    fi
    printf 'Executable preflight passed: %s\n' "$component"
done

if ! smoke_output="$(cargo test --offline -p agalma-conductor \
    --test ledger_process --test workspace_process --test taskqueue_process \
    --test execution_process --test sandbox_process \
    live_process_smoke -- --nocapture --test-threads=1 2>&1)"; then
    printf '%s\n' "$smoke_output" >&2
    exit 1
fi
printf '%s\n' "$smoke_output"
if [[ "$(printf '%s\n' "$smoke_output" | rg -c 'test result: ok\. 1 passed; 0 failed; 0 ignored;')" != 5 ]]; then
    printf 'Required smoke evidence must include five executed, passing cases\n' >&2
    exit 1
fi
if [[ "$smoke_output" == *'skip:'* ]]; then
    printf 'Required smoke evidence must not contain skipped cases\n' >&2
    exit 1
fi

cargo test --offline -p agalma-conductor -- --test-threads=1
