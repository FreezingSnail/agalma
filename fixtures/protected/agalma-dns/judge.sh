#!/usr/bin/env bash
# Frozen protected behavioral judge for the agalma-dns evidence-path fix.
#
# The candidate cannot supply its own judge: this file lives in the operator
# protected baseline and is materialized outside the candidate tree. It sources
# the candidate's *actual* scripts/test.sh (via AGALMA_CANDIDATE_DIR) and drives
# the public focused-runner functions directly, so no candidate-authored test is
# the authority.
#
# It independently proves that a selected job's stdout log and exit-status file
# land at the evidence path implied by RUN_ROOT even when the job changes cwd,
# for:
#   1. an explicit relative RUN_ROOT (the observed broken-primary defect),
#   2. the default root (resolved like test.sh::main),
#   3. an absolute root containing a space.
#
# Every probe runs in a bounded child. The broken runner never writes its status
# file, so the runner's own pool waits forever; the watchdog terminates the
# probe and the judge reports red instead of hanging.
#
# Evidence is append-only. Every invocation gets a unique timestamp+PID run id
# and a unique scratch/evidence subdirectory; nothing is deleted or reused, so a
# retry can never destroy an earlier judgment. The default evidence base is the
# protected root, which survives candidate worktree cleanup; the emitted dns
# policy supplies the primary persistent target/test-runs/agalma-dns base.
#
# Environment:
#   AGALMA_CANDIDATE_DIR     required; candidate repository root
#   AGALMA_PROTECTED_DIR     required; this file's materialized baseline root
#   AGALMA_JUDGE_EVIDENCE_BASE
#                            optional persistent evidence base; default
#                            <protected>/judge-evidence
#   AGALMA_JUDGE_TIMEOUT     optional per-probe seconds (default 20)
#   AGALMA_JUDGE_SCRATCH     optional scratch base; default <evidence>/scratch
#   AGALMA_JUDGE_BEAD        optional evidence label (default agalma-dns-judge)
#
# Exit 0 only when all probes deliver a log and status and complete.

set -u

cand="${AGALMA_CANDIDATE_DIR:?AGALMA_CANDIDATE_DIR is required}"
protected="${AGALMA_PROTECTED_DIR:?AGALMA_PROTECTED_DIR is required}"
limit="${AGALMA_JUDGE_TIMEOUT:-20}"
evidence="${AGALMA_JUDGE_EVIDENCE_BASE:-$protected/judge-evidence}"
scratch_base="${AGALMA_JUDGE_SCRATCH:-$evidence/scratch}"
bead="${AGALMA_JUDGE_BEAD:-agalma-dns-judge}"
invocation="$(date -u +%Y%m%dT%H%M%SZ)-$$"
run_root="$scratch_base/invocation-$invocation"

if [ ! -f "$cand/scripts/test.sh" ]; then
    echo "judge: candidate has no scripts/test.sh under $cand" >&2
    exit 1
fi

# Append-only: create this invocation's roots, never remove a prior run.
mkdir -p "$run_root"
mkdir -p "$evidence"

failures=0
passes=0

kill_tree() {
    local pid="$1"
    local sig="$2"
    local child
    for child in $(pgrep -P "$pid" 2>/dev/null || true); do
        kill_tree "$child" "$sig"
    done
    kill "-$sig" "$pid" 2>/dev/null || true
}

# run_bounded <outfile> <timed-out-marker> <command...>
run_bounded() {
    local out="$1"
    local marker="$2"
    local pid watcher rc
    shift 2
    rm -f "$marker"
    ( "$@" ) >"$out" 2>&1 &
    pid=$!
    (
        sleep "$limit"
        : >"$marker"
        kill_tree "$pid" TERM
        sleep 1
        kill_tree "$pid" KILL
    ) &
    watcher=$!
    wait "$pid"
    rc=$?
    kill "$watcher" 2>/dev/null || true
    wait "$watcher" 2>/dev/null || true
    return "$rc"
}

# probe <name> <mode> <expected-root> <run-cwd>
# mode: relative | default | absolute
probe() {
    local name="$1"
    local mode="$2"
    local expected_root="$3"
    local run_cwd="$4"
    local case_root="$run_root/$name"
    local observed="$case_root/observed-cwd"
    local job="$case_root/job.sh"
    local child="$case_root/probe.sh"
    local run_id="run-$invocation-$name"
    local out="$case_root/probe.log"
    local marker="$case_root/timed-out"
    local probe_rc failure log status

    mkdir -p "$case_root/job-cwd" "$run_cwd"

    cat >"$job" <<'EOF'
#!/bin/sh
set -eu
printf 'judge-job-ok\n'
pwd > "$AGALMA_JUDGE_OBSERVED"
EOF
    chmod 755 "$job"

    cat >"$child" <<'EOF'
#!/usr/bin/env bash
set -u
cand="$1"
mode="$2"
case_root="$3"
job="$4"
observed="$5"
run_cwd="$6"
bead="$7"
run_id="$8"
cd "$run_cwd" || exit 90
. "$cand/scripts/test.sh"
SCOPE=focused
BEAD="$bead"
RUN_ID="$run_id"
case "$mode" in
    relative) RUN_ROOT=evidence-root ;;
    absolute) RUN_ROOT="$case_root/absolute evidence root" ;;
    default)  RUN_ROOT="$(default_run_root)" ;;
esac
init_run_dir || exit 91
JOB_COUNT=1
JOB_LABEL[0]=judge
JOB_CWD[0]="$case_root/job-cwd"
JOB_EXE[0]="$job"
JOB_ARGV[0]=""
JOB_KEYS[0]=""
JOB_CONSERVATIVE[0]=1
JOB_LOG[0]="$RUN_DIR/jobs/00-judge.log"
JOB_DISCOVERED[0]=1
EFF_JOBS=1
JOB_STATUS_DIR="$RUN_DIR/jobs"
mkdir -p "$JOB_STATUS_DIR" || exit 92
AGALMA_JUDGE_OBSERVED="$observed" run_pool
printf '%s\n' "${JOB_RC[0]:-missing}" > "$case_root/rc"
EOF
    chmod 755 "$child"

    run_bounded "$out" "$marker" bash "$child" "$cand" "$mode" "$case_root" "$job" "$observed" "$run_cwd" "$bead" "$run_id"
    probe_rc=$?

    log="$expected_root/$bead/$run_id/jobs/00-judge.log"
    status="$expected_root/$bead/$run_id/jobs/0.status"
    failure=""

    [ -e "$marker" ] && failure="probe exceeded ${limit}s (missing status delivery)"
    [ -z "$failure" ] && [ "$probe_rc" -ne 0 ] && failure="probe exit $probe_rc"
    [ -z "$failure" ] && { [ -f "$case_root/rc" ] || failure="probe never completed"; }
    [ -z "$failure" ] && [ "$(cat "$case_root/rc" 2>/dev/null)" != "0" ] && \
        failure="job rc $(cat "$case_root/rc" 2>/dev/null) != 0"
    [ -z "$failure" ] && { [ -f "$log" ] || failure="missing job log at $log"; }
    [ -z "$failure" ] && ! grep -q 'judge-job-ok' "$log" 2>/dev/null && \
        failure="job log does not contain its output"
    [ -z "$failure" ] && { [ -f "$status" ] || failure="missing job status at $status"; }
    [ -z "$failure" ] && [ "$(cat "$status" 2>/dev/null)" != "0" ] && \
        failure="job status is not 0"
    [ -z "$failure" ] && [ ! -f "$observed" ] && failure="job cwd was not observed"
    [ -z "$failure" ] && [ "$(cat "$observed" 2>/dev/null)" != "$case_root/job-cwd" ] && \
        failure="job did not run in the changed cwd"

    if [ -z "$failure" ]; then
        passes=$((passes + 1))
        printf 'PASS %s: log=%s status=%s\n' "$name" "$log" "$status"
    else
        failures=$((failures + 1))
        printf 'FAIL %s: %s\n' "$name" "$failure" >&2
        printf '%s\n' "---- probe output ($name) ----" >&2
        sed -n '1,80p' "$out" >&2 2>/dev/null || true
    fi
}

echo "judge: candidate=$cand evidence=$evidence scratch=$scratch_base invocation=$invocation timeout=${limit}s"
probe relative-root relative \
    "$run_root/relative-root/evidence-root" \
    "$run_root/relative-root"
# Default: call default_run_root from a non-repository invoking directory. A
# primary checkout's relative default is interpreted against that invoking
# directory, while the path stays unique and persistent under the evidence base.
probe default-root default \
    "$evidence/default-root/invocation-$invocation/target/test-runs" \
    "$evidence/default-root/invocation-$invocation"
probe absolute-root absolute \
    "$run_root/absolute-root/absolute evidence root" \
    "$run_root/absolute-root"

echo "judge: $passes passed, $failures failed"
[ "$failures" -eq 0 ]
