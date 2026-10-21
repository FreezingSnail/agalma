#!/usr/bin/env bash
# Independently specified positive-control focused runner for the protected
# agalma-dns judge. This is NOT a fix for the product `scripts/test.sh`; it is a
# saved, deterministic control implementation of the judge's expected contract:
#
# - `default_run_root` returns an absolute, unique evidence base;
# - `init_run_dir` resolves a relative RUN_ROOT against the invoking directory;
# - `run_pool` may change each job's cwd while still writing the job log and
#   status file under the absolute RUN_DIR.
#
# The protected judge must accept this runner for all probe modes.

set -o pipefail

default_run_root() {
    printf '%s/target/test-runs\n' "${AGALMA_CONTROL_DEFAULT_BASE:-$PWD}"
}

init_run_dir() {
    if [ -z "${RUN_ID:-}" ]; then RUN_ID="$(date -u +%Y%m%dT%H%M%SZ)-$$"; fi
    local base="${RUN_ROOT:-$(default_run_root)}"
    case "$base" in
        /*) ;;
        *) base="$PWD/$base" ;;
    esac
    RUN_DIR="$base/$BEAD/$RUN_ID"
    mkdir -p "$RUN_DIR" || return 1
    printf '%s\n' "$RUN_DIR" > "$base/$BEAD/.last-run" || return 1
}

run_pool() {
    local j=0 rc
    while [ "$j" -lt "${JOB_COUNT:-0}" ]; do
        local log="${JOB_LOG[$j]}"
        local status="$JOB_STATUS_DIR/$j.status"
        local cwd="${JOB_CWD[$j]}"
        local exe="${JOB_EXE[$j]}"
        local -a argv=()
        local line
        while IFS= read -r line; do
            [ -n "$line" ] && argv[${#argv[@]}]="$line"
        done <<< "${JOB_ARGV[$j]}"
        (
            cd "$cwd" || exit 90
            "$exe" ${argv[@]+"${argv[@]}"} > "$log" 2>&1
            rc=$?
            printf '%s\n' "$rc" > "$status"
            exit "$rc"
        ) &
        local pid=$!
        wait "$pid"
        rc=$?
        JOB_RC[$j]="$rc"
        j=$((j + 1))
    done
}
