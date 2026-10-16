#!/usr/bin/env bash
# Durable regression check for the `agalma-ipc` no-op rebuild property.
#
#   bash scripts/check-noop-rebuild.sh
#
# Why this exists
#   `crates/agalma-ipc/build.rs` writes a descriptor set to
#   `$OUT_DIR/ipc.fds` and hands it to `connectrpc_build::Config::descriptor_set`.
#   In Precompiled mode connectrpc-build registers
#   `cargo:rerun-if-changed=<that path>`; because the build script rewrites the
#   file every run, cargo saw a watched input change and reran the build script
#   — plus every dependent crate — on EVERY invocation. The fix suppresses the
#   generated directives and keeps the script's own `ipc.proto`/`build.rs`
#   directives as the complete rerun set.
#
# What it asserts
#   1. Two consecutive identical `cargo build -p agalma-ipc`: the second is a
#      no-op (no `Compiling agalma-ipc`, Finished in < 0.3s).
#   2. Two consecutive identical
#      `cargo test -p agalma-conductor --test sf_budget --no-run`: the second
#      compiles nothing.
#   3. The no-op is caused by *correct* inputs, not by disabling rerun: editing
#      `proto/ipc.proto` and editing `build.rs` each still trigger a rebuild.
#
# Environment: BEAD (evidence label, default agalma-bdx), RUN_ROOT, RUN_ID,
#   CARGO, CARGO_FLAGS (default --offline --locked), CARGO_TARGET_DIR,
#   CARGO_INCREMENTAL (default 0). Inside a worktree, set CARGO_TARGET_DIR to
#   target/worktree-caches/<bead>/target. Source edits made by check 3 are
#   always restored (via trap) even if a check fails.

set -o pipefail

repo_root="$(cd -- "$(dirname -- "${BASH_SOURCE[0]}")/.." && pwd)"
cd "$repo_root" || exit 1

# Reuse the unified runner's run-root resolution (primary worktree's
# target/test-runs) so evidence lands at the persistent root, not worktree-local.
# shellcheck source=test.sh
source "$repo_root/scripts/test.sh"

CARGO="${CARGO:-cargo}"
CARGO_FLAGS="${CARGO_FLAGS:---offline --locked}"
export CARGO_INCREMENTAL="${CARGO_INCREMENTAL:-0}"

BEAD="${BEAD:-agalma-bdx}"
RUN_ROOT="${RUN_ROOT:-$(default_run_root)}"
RUN_ID="${RUN_ID:-$(date -u +%Y%m%dT%H%M%SZ)-$$}"
RUN_DIR="$RUN_ROOT/$BEAD/$RUN_ID"
mkdir -p "$RUN_DIR"

BUILD_RS="$repo_root/crates/agalma-ipc/build.rs"
PROTO="$repo_root/crates/agalma-ipc/proto/ipc.proto"
backup_dir="$RUN_DIR/backup"
mkdir -p "$backup_dir"
cp "$BUILD_RS" "$backup_dir/build.rs.orig"
cp "$PROTO" "$backup_dir/ipc.proto.orig"

restore_sources() {
    cp "$backup_dir/build.rs.orig" "$BUILD_RS"
    cp "$backup_dir/ipc.proto.orig" "$PROTO"
}
trap restore_sources EXIT

fail=0
pass() { printf 'PASS  %s\n' "$1"; }
bad() { printf 'FAIL  %s\n' "$1"; fail=1; }

# Cargo's own reported elapsed seconds from the final `Finished ... in X.XXs`
# line; portable across macOS/Linux (no reliance on `date +%N`).
finished_secs() {
    sed -n 's/.*Finished .* in \([0-9][0-9]*\.[0-9][0-9]*\)s$/\1/p' "$1" | tail -1
}

printf 'check-noop-rebuild: bead=%s run=%s\n' "$BEAD" "$RUN_ID"
printf '  repo=%s\n  target=%s\n  evidence=%s\n' \
    "$repo_root" "${CARGO_TARGET_DIR:-<default target>}" "$RUN_DIR"

echo
echo '[1/3] library no-op: cargo build -p agalma-ipc (x2)'
"$CARGO" build $CARGO_FLAGS -p agalma-ipc >"$RUN_DIR/build-prime.log" 2>&1 \
    || { bad "prime build failed (see build-prime.log)"; }
"$CARGO" build $CARGO_FLAGS -p agalma-ipc >"$RUN_DIR/build-1.log" 2>&1
"$CARGO" build $CARGO_FLAGS -p agalma-ipc >"$RUN_DIR/build-2.log" 2>&1
c2="$(grep -c 'Compiling agalma-ipc' "$RUN_DIR/build-2.log" || true)"
t2="$(finished_secs "$RUN_DIR/build-2.log")"
if [ "$c2" = "0" ]; then
    pass "second build did not compile agalma-ipc"
else
    bad "second build recompiled agalma-ipc ($c2 Compiling line(s))"
fi
if [ -n "$t2" ] && awk -v t="$t2" 'BEGIN { exit !(t < 0.3) }'; then
    pass "second build Finished in ${t2}s (< 0.3s)"
else
    bad "second build not a fast no-op (Finished in ${t2:-?}s)"
fi

echo
echo '[2/3] dependents no-op: cargo test -p agalma-conductor --test sf_budget --no-run (x2)'
"$CARGO" test $CARGO_FLAGS -p agalma-conductor --test sf_budget --no-run \
    >"$RUN_DIR/sfbudget-prime.log" 2>&1 \
    || { bad "prime sf_budget build failed (see sfbudget-prime.log)"; }
"$CARGO" test $CARGO_FLAGS -p agalma-conductor --test sf_budget --no-run \
    >"$RUN_DIR/sfbudget-1.log" 2>&1
"$CARGO" test $CARGO_FLAGS -p agalma-conductor --test sf_budget --no-run \
    >"$RUN_DIR/sfbudget-2.log" 2>&1
cc2="$(grep -c '^ *Compiling' "$RUN_DIR/sfbudget-2.log" || true)"
if [ "$cc2" = "0" ]; then
    pass "second sf_budget --no-run compiled nothing downstream"
else
    bad "second sf_budget --no-run recompiled $cc2 target(s)"
fi

echo
echo '[3/3] correct inputs still trigger a rebuild'

# Restore, then touch build.rs -> expect a rebuild.
cp "$backup_dir/build.rs.orig" "$BUILD_RS"
printf '\n// noop-rebuild regression probe\n' >>"$BUILD_RS"
"$CARGO" build $CARGO_FLAGS -p agalma-ipc >"$RUN_DIR/trigger-buildrs.log" 2>&1
if [ "$(grep -c 'Compiling agalma-ipc' "$RUN_DIR/trigger-buildrs.log" || true)" != "0" ]; then
    pass "editing build.rs triggered a rebuild"
else
    bad "editing build.rs did NOT trigger a rebuild (rerun set too narrow)"
fi
cp "$backup_dir/build.rs.orig" "$BUILD_RS"
"$CARGO" build $CARGO_FLAGS -p agalma-ipc >"$RUN_DIR/settle.log" 2>&1

# Restore, then touch ipc.proto -> expect a rebuild.
cp "$backup_dir/ipc.proto.orig" "$PROTO"
printf '\n// noop-rebuild regression probe\n' >>"$PROTO"
"$CARGO" build $CARGO_FLAGS -p agalma-ipc >"$RUN_DIR/trigger-proto.log" 2>&1
if [ "$(grep -c 'Compiling agalma-ipc' "$RUN_DIR/trigger-proto.log" || true)" != "0" ]; then
    pass "editing ipc.proto triggered a rebuild"
else
    bad "editing ipc.proto did NOT trigger a rebuild"
fi
cp "$backup_dir/ipc.proto.orig" "$PROTO"

echo
if [ "$fail" -eq 0 ]; then
    printf 'RESULT: PASS (no-op rebuild property holds)\n'
else
    printf 'RESULT: FAIL (see %s)\n' "$RUN_DIR"
fi
exit "$fail"
