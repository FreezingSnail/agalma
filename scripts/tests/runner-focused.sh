#!/usr/bin/env bash
# End-to-end acceptance checks for the focused runner (scripts/test.sh).
#
# Each case invokes the runner as a user would (bash scripts/test.sh ...) and
# asserts on the recorded summary and exit status. Evidence for every case is
# kept under $RUN_ROOT/$BEAD/$RUN_ID_BASE/ at the persistent artifact root.
#
# Requires the workspace server binaries for the process/crash case; the runner
# builds them through --prereq-bins.

set -o pipefail

repo_root="$(cd -- "$(dirname -- "${BASH_SOURCE[0]}")/../.." && pwd)"
cd "$repo_root" || exit 1

# shellcheck source=../test.sh
source "$repo_root/scripts/test.sh"

BEAD="${BEAD:-agalma-i15}"
RUN_ROOT="${RUN_ROOT:-$(default_run_root)}"
RUN_ID_BASE="${RUN_ID_BASE:-int-$(date -u +%Y%m%dT%H%M%SZ)-$$}"
OUT="$RUN_ROOT/$BEAD/$RUN_ID_BASE/integration"
mkdir -p "$OUT"

PASS=0
FAIL=0
FAILED=""
CASE=0
LAST_RC=0
LAST_OUT=""
LAST_RUN_DIR=""
RUNNER_JOBS=2

check() {
    local desc="$1"; shift
    if "$@"; then
        PASS=$((PASS + 1))
    else
        FAIL=$((FAIL + 1))
        FAILED="$FAILED
  - $desc"
        echo "FAIL: $desc" >&2
    fi
}

run_runner() {
    CASE=$((CASE + 1))
    local rid
    rid="$(printf '%s-int-%02d' "$RUN_ID_BASE" "$CASE")"
    LAST_OUT="$OUT/case-$(printf '%02d' "$CASE").log"
    LAST_RUN_DIR="$RUN_ROOT/$BEAD/$rid"
    rm -rf "$LAST_RUN_DIR"
    printf '%s\n' "bash scripts/test.sh $*" > "$OUT/case-$(printf '%02d' "$CASE").command"
    BEAD="$BEAD" RUN_ROOT="$RUN_ROOT" RUN_ID="$rid" JOBS="$RUNNER_JOBS" \
        bash "$repo_root/scripts/test.sh" "$@" > "$LAST_OUT" 2>&1
    LAST_RC=$?
}

sf() { jq -r "$1" "$LAST_RUN_DIR/summary.json" 2>/dev/null; }

job_first_label() { sf ".targets[0].label"; }

# ---------------------------------------------------------------------------
echo "[integration] case 1: empty focused fails before any build"
run_runner focused
check "empty focused exit=2" test "$LAST_RC" -eq 2
check "empty focused created no run dir" test ! -d "$LAST_RUN_DIR"

echo "[integration] case 2: invalid crate fails"
run_runner focused -p nope --test t
check "invalid crate exit=2" test "$LAST_RC" -eq 2
check "invalid crate created no run dir" test ! -d "$LAST_RUN_DIR"

echo "[integration] case 3: unit without a target fails"
run_runner focused -p agalma-contracts
check "no target exit=2" test "$LAST_RC" -eq 2

echo "[integration] case 4: filter matching no required cases fails"
run_runner focused -p agalma-contracts --isolated --test digest no_such_filter_zzz
check "zero-case exit=1" test "$LAST_RC" -eq 1
check "zero-case status failed" test "$(sf .status)" = failed
check "zero-case discovered 0" test "$(sf .targets[0].discovered)" = 0

echo "[integration] case 5: ignored-only required selection cannot pass"
run_runner focused -p agalma-conductor --isolated --test provider_proxy_live \
    confined_turn_reaches_provider_through_parent_proxy
check "ignored-only exit=1" test "$LAST_RC" -eq 1
check "ignored-only status failed" test "$(sf .status)" = failed
check "ignored-only discovered 1" test "$(sf .targets[0].discovered)" = 1
check "ignored-only passed 0" test "$(sf .targets[0].passed)" = 0
check "ignored-only ignored 1" test "$(sf .targets[0].ignored)" = 1

echo "[integration] case 6: multi-crate target-specific filters, overlap jobs=2"
run_runner focused \
    -p agalma-contracts --isolated --test digest float \
    -p agalma-workspace --isolated --test workspace
check "multi-crate exit=0" test "$LAST_RC" -eq 0
check "multi-crate status passed" test "$(sf .status)" = passed
check "digest filter selected 1 case" test "$(sf .targets[0].discovered)" = 1
check "digest passed 1" test "$(sf .targets[0].passed)" = 1
check "workspace passed nonzero" test "$(sf .targets[1].passed)" -gt 0
check "multi-crate peak 2 (independent overlap)" test "$(sf .peak_concurrency)" = 2
check "compile duration recorded" test "$(sf .compile_ms_sum)" -gt 0
CASE6_PEAK="$(sf .peak_concurrency)"
CASE6_COMPILE="$(sf .compile_ms_sum)"
CASE6_EXEC="$(sf .exec_ms_sum)"
CASE6_DIGEST="$(sf .targets[0].passed)"
CASE6_WORKSPACE="$(sf .targets[1].passed)"

echo "[integration] case 7: unrelated target neither built nor executed"
# Case 7 reuses case 6's run directory (no new runner invocation). It still
# consumes a case number so file names and run ids track the printed labels.
CASE=$((CASE + 1))
CASE7_DIR="$LAST_RUN_DIR"
check "no unrelated build json" bash -c "! ls '$CASE7_DIR'/build/*.json | grep -q agalma-conductor"
check "only selected crates built" bash -c "[ \$(ls '$CASE7_DIR'/build/*.json | wc -l | tr -d ' ') = 2 ]"
check "only selected targets ran" bash -c "[ \$(ls '$CASE7_DIR'/jobs/*.log | wc -l | tr -d ' ') = 2 ]"

echo "[integration] case 8: filter selectivity proves target-specific filter"
run_runner focused -p agalma-contracts --isolated --test digest
check "unfiltered digest exit=0" test "$LAST_RC" -eq 0
check "unfiltered digest discovered > filtered" bash -c "[ \$(jq -r '.targets[0].discovered' '$LAST_RUN_DIR/summary.json') -gt 1 ]"

echo "[integration] case 9: failure propagates while another selection passes"
run_runner focused \
    -p agalma-contracts --isolated --test digest float \
    -p agalma-workspace --isolated --test workspace no_such_filter_zzz
check "propagation exit=1" test "$LAST_RC" -eq 1
check "propagation status failed" test "$(sf .status)" = failed
check "passing selection still executed" test "$(sf .targets[0].passed)" = 1

echo "[integration] case 10: group exclusion serializes"
run_runner focused \
    -p agalma-contracts --group res --test digest float \
    -p agalma-workspace --group res --test workspace
check "same group exit=0" test "$LAST_RC" -eq 0
check "same group peak 1" test "$(sf .peak_concurrency)" = 1

echo "[integration] case 11: unknown isolation serializes conservatively"
run_runner focused \
    -p agalma-contracts --test digest float \
    -p agalma-workspace --test workspace
check "conservative exit=0" test "$LAST_RC" -eq 0
check "conservative peak 1" test "$(sf .peak_concurrency)" = 1

echo "[integration] case 12: jobs=1 yields equivalent results"
RUNNER_JOBS=1
run_runner focused \
    -p agalma-contracts --isolated --test digest float \
    -p agalma-workspace --isolated --test workspace
RUNNER_JOBS=2
check "jobs=1 exit=0" test "$LAST_RC" -eq 0
check "jobs=1 peak 1" test "$(sf .peak_concurrency)" = 1
check "jobs=1 digest equivalent" test "$(sf .targets[0].passed)" = "$CASE6_DIGEST"
check "jobs=1 workspace equivalent" test "$(sf .targets[1].passed)" = "$CASE6_WORKSPACE"
CASE12_COMPILE="$(sf .compile_ms_sum)"
CASE12_EXEC="$(sf .exec_ms_sum)"
CASE12_PEAK="$(sf .peak_concurrency)"

echo "[integration] case 13: process/crash selection through focused path, parity"
run_runner focused -p agalma-execution --isolated --test crash_matrix \
    crash_after_transition_commit_before_dispatch
check "crash exit=0" test "$LAST_RC" -eq 0
check "crash passed 1" test "$(sf .targets[0].passed)" = 1
# Environment parity against the documented scoped Cargo command.
PARITY_OUT="$OUT/parity-crash-matrix.log"
CARGO_TARGET_DIR="${CARGO_TARGET_DIR:-target}" "$CARGO" test --offline --locked \
    -p agalma-execution --test crash_matrix \
    crash_after_transition_commit_before_dispatch -- --test-threads=1 > "$PARITY_OUT" 2>&1
PARITY_RC=$?
check "parity cargo command ok" test "$PARITY_RC" -eq 0
check "parity cargo 1 passed" bash -c "grep -q 'test result: ok. 1 passed;' '$PARITY_OUT'"

echo "[integration] case 14: process smoke with built prerequisites"
run_runner focused -p agalma-conductor --prereq-bins --isolated \
    --test ledger_process live_process_smoke
check "process smoke exit=0" test "$LAST_RC" -eq 0
check "process smoke passed 1" test "$(sf .targets[0].passed)" = 1
# Prerequisite evidence must exist and be non-empty. Regression: it was written
# before $RUN_DIR/build existed and the write failure was swallowed by the pipe.
check "prereq command names workspace bins" bash -c \
    "grep -q -- '--workspace --bins' '$LAST_RUN_DIR/build/prereq-workspace-bins.command'"
check "prereq log non-empty" bash -c \
    "[ -s '$LAST_RUN_DIR/build/prereq-workspace-bins.log' ]"
check "prereq status non-empty" bash -c \
    "[ -s '$LAST_RUN_DIR/build/prereq-workspace-bins.status' ]"
check "prereq duration non-empty" bash -c \
    "[ -s '$LAST_RUN_DIR/build/prereq-workspace-bins.duration' ]"
check "prereq log glob matches" bash -c \
    "[ \$(ls '$LAST_RUN_DIR'/build/prereq-*.log | wc -l | tr -d ' ') -ge 1 ]"

echo "[integration] case 15: interruption records incomplete and reaps children"
CASE=$((CASE + 1))
INT_RID="$(printf '%s-int-%02d' "$RUN_ID_BASE" "$CASE")"
INT_DIR="$RUN_ROOT/$BEAD/$INT_RID"
rm -rf "$INT_DIR"
INT_OUT="$OUT/case-$(printf '%02d' "$CASE").log"
BEAD="$BEAD" RUN_ROOT="$RUN_ROOT" RUN_ID="$INT_RID" JOBS=2 \
    bash "$repo_root/scripts/test.sh" focused \
    -p agalma-conductor --isolated --test ledger_process live_process_smoke \
    -p agalma-conductor --isolated --test workspace_process live_process_smoke \
    > "$INT_OUT" 2>&1 &
INT_PID=$!
# Wait for the execution phase (a job log appears), then interrupt.
for _ in $(seq 1 1200); do
    [ -n "$(find "$INT_DIR/jobs" -name '*.log' 2>/dev/null | head -1)" ] && break
    sleep 0.02
done
sleep 0.05
kill -TERM "$INT_PID" 2>/dev/null
wait "$INT_PID"
INT_RC=$?
check "interruption exit=130" test "$INT_RC" -eq 130
check "interruption status incomplete" test "$(jq -r .status "$INT_DIR/summary.json" 2>/dev/null)" = incomplete
check "interruption logs preserved" test -f "$INT_DIR/summary.json"
sleep 0.3
check "interruption reaped children" bash -c \
    "! pgrep -f 'ledger_process-[0-9a-f]+ live_process_smoke|workspace_process-[0-9a-f]+ live_process_smoke' >/dev/null"

echo "[integration] case 16: --exact narrows discovery to the executed case"
# `execution_client::tests::missing_socket_is_unavailable` is a substring of the
# `..._over_connect` test name, so without --exact at discovery the recorded
# count is 2 while the run executes 1. The runner must apply --exact during
# discovery for units that carry it.
run_runner focused -p agalma-ipc --isolated --lib \
    "execution_client::tests::missing_socket_is_unavailable" --exact
check "exact exit=0" test "$LAST_RC" -eq 0
check "exact discovered 1" test "$(sf .targets[0].discovered)" = 1
check "exact passed 1" test "$(sf .targets[0].passed)" = 1

echo "[integration] case 17: peers/full contracts retained"
PEERS_PRINT="$(bash "$repo_root/scripts/test.sh" peers --print 2>&1)"
check "peers prints 3 commands" bash -c "[ \$(printf '%s\n' \"\$1\" | grep -c '^cargo') = 3 ]" _ "$PEERS_PRINT"
check "peers builds workspace bins" bash -c "printf '%s\n' \"\$1\" | grep -q 'build --offline --workspace --bins'" _ "$PEERS_PRINT"
check "peers runs five smokes" bash -c "printf '%s\n' \"\$1\" | grep -q 'ledger_process --test workspace_process'" _ "$PEERS_PRINT"
FULL_PRINT="$(bash "$repo_root/scripts/test.sh" full --print 2>&1)"
check "full prints 2 commands" bash -c "[ \$(printf '%s\n' \"\$1\" | grep -c '^cargo') = 2 ]" _ "$FULL_PRINT"
VERIFY_PRINT="$(bash "$repo_root/scripts/verify-peer-transports.sh" --print 2>&1)"
check "verify-peer forwards to peers" bash -c "[ \"\$1\" = \"\$2\" ]" _ "$VERIFY_PRINT" "$PEERS_PRINT"

# ---------------------------------------------------------------------------
{
    echo "# focused-runner integration $RUN_ID_BASE"
    echo
    echo "- bead: $BEAD"
    echo "- concrete cases (no generation)"
    echo "- pass: $PASS"
    echo "- fail: $FAIL"
    echo
    echo "## durations (case 6 vs case 12; same selection, no speedup claim)"
    echo "- digest passed (filtered): $CASE6_DIGEST"
    echo "- workspace passed (all): $CASE6_WORKSPACE"
    echo "- jobs=2: compile_ms=$CASE6_COMPILE exec_ms=$CASE6_EXEC peak=$CASE6_PEAK"
    echo "- jobs=1: compile_ms=$CASE12_COMPILE exec_ms=$CASE12_EXEC peak=$CASE12_PEAK"
} > "$OUT/integration-receipt.txt"
{
    printf 'bead=%s\nrun_id_base=%s\npass=%s\nfail=%s\n' "$BEAD" "$RUN_ID_BASE" "$PASS" "$FAIL"
    [ -n "$FAILED" ] && printf 'failed:%s\n' "$FAILED"
} >> "$OUT/integration-receipt.txt"

echo "[integration] pass=$PASS fail=$FAIL evidence=$OUT"
[ "$FAIL" -eq 0 ] || exit 1
exit 0
