#!/usr/bin/env bash
# Unified test entry point (scope contract: docs/development-workflow.md;
# selection + concurrency contract: docs/testing-strategy.md).
#
#   bash scripts/test.sh focused [selection...]   explicit target/module checks
#   bash scripts/test.sh peers                     local process smokes + suite
#   bash scripts/test.sh full                      workspace tests
#   bash scripts/test.sh help                      usage (no build, no tests)
#
# Scopes are limited to focused, peers, and full. A focused request must name an
# explicit crate and target/module per selection unit; it is never evaluated as
# shell code. Selected test executables are compiled once in the stable Cargo
# cache, then executed directly through a bounded worker pool. Evidence (exact
# command, raw log, exit status, compile/run durations, resource group, and
# observed peak concurrency) is written under
# target/test-runs/<bead>/<run-id>/.
#
# Compatibility: scripts/verify-peer-transports.sh forwards here for `peers`.
#
# Portability: /bin/bash on macOS is 3.2. This script avoids bash-4 features
# (no associative arrays, no `mapfile`, no `wait -n`, no `${var,,}`). It is
# sourceable: `main` runs only when the file is executed, so tests can exercise
# the parser, scheduler, and aggregation functions directly.

set -o pipefail

# ---------------------------------------------------------------------------
# Configuration and state
# ---------------------------------------------------------------------------
SCOPE=""
PRINT_ONLY=0
BEAD="${BEAD:-manual}"
RUN_ROOT="${RUN_ROOT:-}"
RUN_ID="${RUN_ID:-}"
CARGO="${CARGO:-cargo}"
CARGO_FLAGS="${CARGO_FLAGS:---offline --locked}"
CARGO_INCREMENTAL="${CARGO_INCREMENTAL:-0}"
export CARGO_INCREMENTAL
JOBS_REQUESTED="${JOBS:-2}"
THREADS_REQUESTED="${THREADS:-}"

RUN_DIR=""
JOB_STATUS_DIR=""
PEAK_CONCURRENCY=0
INTERRUPTED=0
CPU_BUDGET=1
EFF_JOBS=1
EFF_THREADS=1
DISCOVERED_CASES=0
SEL_ARGS=()

# Selection units (parallel arrays), populated by parse_selection.
UNIT_COUNT=0
UNIT_CRATE=()
UNIT_KIND=()         # lib|test|bin|example|bench
UNIT_TNAME=()        # target name for test/bin/example/bench ("" for lib)
UNIT_FILTER=()       # space-joined filter tokens ("" = whole target)
UNIT_EXACT=()        # 0/1
UNIT_KEYS=()         # ";group:a;group:b" exclusive keys ("" = none)
UNIT_CONSERVATIVE=() # 1 when isolation is unknown (serialize with everything)

# Job plan (parallel arrays), populated by build_jobs.
JOB_COUNT=0
JOB_LABEL=()
JOB_CWD=()
JOB_EXE=()
JOB_ARGV=()          # newline-joined argument tokens
JOB_KEYS=()
JOB_CONSERVATIVE=()
JOB_LOG=()
JOB_RC=()
JOB_START_MS=()
JOB_END_MS=()
JOB_OVERLAP=()       # newline-joined indices running when this job launched
JOB_DISCOVERED=()
JOB_PASSED=()
JOB_FAILED=()
JOB_IGNORED=()
JOB_OK=()
JOB_PID=()
JOB_UNRUNNABLE=()    # 1 when discovery rejected the unit (never launched)

# Worker-pool runtime state (global so the signal handler can reach it).
POOL_RUNNING=()
POOL_STATE=()

# Aggregates.
AGG_STATUS="failed"
AGG_PASSED=0
AGG_FAILED=0
AGG_IGNORED=0

# Compiled executable index.
EXE_CRATE=(); EXE_KIND=(); EXE_NAME=(); EXE_PATH=(); EXE_MANIFEST=()

# Workspace metadata.
WORKSPACE_CRATES=()
TGT_ENTRIES=()       # "crate|kind|name"

# ---------------------------------------------------------------------------
# Utilities
# ---------------------------------------------------------------------------

# Milliseconds since the epoch. Prefer nanosecond output, fall back to seconds.
now_ms() {
    local t
    t="$(date -u +%s%N 2>/dev/null || true)"
    if [[ "$t" =~ ^[0-9]+$ ]] && [ "${#t}" -ge 13 ]; then
        printf '%s\n' "$((t / 1000000))"
    else
        printf '%s\n' "$(( $(date -u +%s) * 1000 ))"
    fi
}

# True when the space-delimited list $1 contains the exact word $2.
list_has() {
    local list="$1" needle="$2" item
    for item in $list; do
        [ "$item" = "$needle" ] && return 0
    done
    return 1
}

# True when crate $1 exposes any target of kind $2. Kind is matched against the
# '+'-joined Cargo target kind ("test", "lib", "bin", ...).
crate_has_kind() {
    local crate="$1" kind="$2" entry e_crate e_kind
    for entry in ${TGT_ENTRIES[@]+"${TGT_ENTRIES[@]}"}; do
        e_crate="${entry%%|*}"; [ "$e_crate" = "$crate" ] || continue
        e_kind="${entry#*|}"; e_kind="${e_kind%%|*}"
        case ";$e_kind;" in *";$kind;"*) return 0 ;; esac
    done
    return 1
}

# True when crate $1 exposes target of kind $2 named $3.
target_exists() {
    local crate="$1" kind="$2" name="$3" entry e_crate e_kind e_name
    for entry in ${TGT_ENTRIES[@]+"${TGT_ENTRIES[@]}"}; do
        e_crate="${entry%%|*}"; [ "$e_crate" = "$crate" ] || continue
        e_kind="${entry#*|}"; e_kind="${e_kind%%|*}"
        e_name="${entry##*|}"
        case ";$e_kind;" in *";$kind;"*) ;; *) continue ;; esac
        [ "$e_name" = "$name" ] && return 0
    done
    return 1
}

# Escape a string for embedding in a JSON string literal.
json_escape() {
    local s="$1"
    s="${s//\\/\\\\}"
    s="${s//\"/\\\"}"
    s="${s//$'\n'/\\n}"
    s="${s//$'\t'/\\t}"
    s="${s//$'\r'/\\r}"
    printf '%s' "$s"
}

# True when exclusive-key sets $1 and $2 share a key. Keys are stored as
# ";k1" suffixes so membership can be tested with a glob.
keys_intersect() {
    local a="$1" b="$2" k
    if [ -z "$a" ] || [ -z "$b" ]; then return 1; fi
    IFS=';' read -r -a _ka <<< "$a"
    for k in "${_ka[@]+"${_ka[@]}"}"; do
        [ -z "$k" ] && continue
        case "$b" in *";$k"*) return 0 ;; esac
    done
    return 1
}

# Pure exclusion predicate: do two units with the given keys/conservative flags
# conflict? Conservative units conflict with everything.
exclusion_conflict() {
    local ka="$1" ca="$2" kb="$3" cb="$4"
    if [ "$ca" = "1" ] || [ "$cb" = "1" ]; then return 0; fi
    keys_intersect "${ka};" "${kb};"
}

# Do job indices $1 and $2 conflict?
jobs_conflict() {
    exclusion_conflict "${JOB_KEYS[$1]}" "${JOB_CONSERVATIVE[$1]}" \
        "${JOB_KEYS[$2]}" "${JOB_CONSERVATIVE[$2]}"
}

# ---------------------------------------------------------------------------
# Selection parsing and validation
# ---------------------------------------------------------------------------

# Parse the focused selection tokens in ${SEL_ARGS[@]} into UNIT_* arrays.
# Returns 0 on success, non-zero after printing an error. The token stream is
# validated structurally and never passed to `eval`.
parse_selection() {
    local -a args=(${SEL_ARGS[@]+"${SEL_ARGS[@]}"})
    local n=${#args[@]}
    local i=0

    UNIT_COUNT=0
    UNIT_CRATE=(); UNIT_KIND=(); UNIT_TNAME=(); UNIT_FILTER=()
    UNIT_EXACT=(); UNIT_KEYS=(); UNIT_CONSERVATIVE=()

    local cur_crate="" cur_kind="" cur_tname="" cur_filter="" cur_exact=0
    # Isolation markers apply to the currently open unit; before any unit they
    # apply to the next one opened. A unit with no marker is conservatively
    # unknown and serializes with every other unit.
    local cur_keys="" cur_cons=1 have_unit=0
    local pending_keys="" pending_cons=1

    while [ "$i" -lt "$n" ]; do
        local t="${args[$i]}"
        case "$t" in
            --isolated)
                if [ "$have_unit" = "1" ]; then cur_cons=0; cur_keys=""; else pending_cons=0; pending_keys=""; fi
                i=$((i + 1))
                ;;
            --group)
                if [ $((i + 1)) -ge "$n" ]; then
                    echo "test.sh: --group requires a name" >&2; return 2
                fi
                if [ "$have_unit" = "1" ]; then cur_keys="$cur_keys;group:${args[$((i + 1))]}"; cur_cons=0
                else pending_keys="$pending_keys;group:${args[$((i + 1))]}"; pending_cons=0; fi
                i=$((i + 2))
                ;;
            --group=*)
                if [ "$have_unit" = "1" ]; then cur_keys="$cur_keys;group:${t#--group=}"; cur_cons=0
                else pending_keys="$pending_keys;group:${t#--group=}"; pending_cons=0; fi
                i=$((i + 1))
                ;;
            --prereq)
                [ $((i + 1)) -ge "$n" ] && { echo "test.sh: --prereq requires a crate" >&2; return 2; }
                i=$((i + 2))
                ;;
            --prereq-bins)
                i=$((i + 1))
                ;;
            -p|--package)
                if [ $((i + 1)) -ge "$n" ]; then
                    echo "test.sh: $t requires a crate name" >&2; return 2
                fi
                if [ "$have_unit" = "1" ]; then
                    UNIT_CRATE[$UNIT_COUNT]="$cur_crate"
                    UNIT_KIND[$UNIT_COUNT]="$cur_kind"
                    UNIT_TNAME[$UNIT_COUNT]="$cur_tname"
                    UNIT_FILTER[$UNIT_COUNT]="$cur_filter"
                    UNIT_EXACT[$UNIT_COUNT]="$cur_exact"
                    UNIT_KEYS[$UNIT_COUNT]="$cur_keys"
                    UNIT_CONSERVATIVE[$UNIT_COUNT]="$cur_cons"
                    UNIT_COUNT=$((UNIT_COUNT + 1))
                fi
                cur_crate="${args[$((i + 1))]}"
                cur_kind=""; cur_tname=""; cur_filter=""; cur_exact=0
                cur_keys="$pending_keys"; cur_cons="$pending_cons"
                pending_keys=""; pending_cons=1
                have_unit=1
                i=$((i + 2))
                ;;
            --test|--bin|--example|--bench)
                if [ $((i + 1)) -ge "$n" ]; then
                    echo "test.sh: $t requires a target name" >&2; return 2
                fi
                if [ -n "$cur_kind" ]; then
                    echo "test.sh: each selection unit names exactly one target (saw $t after --$cur_kind)" >&2; return 2
                fi
                cur_kind="${t#--}"
                cur_tname="${args[$((i + 1))]}"
                i=$((i + 2))
                ;;
            --lib)
                if [ -n "$cur_kind" ]; then
                    echo "test.sh: each selection unit names exactly one target (saw --lib after --$cur_kind)" >&2; return 2
                fi
                cur_kind="lib"; cur_tname=""
                i=$((i + 1))
                ;;
            --exact)
                cur_exact=1
                i=$((i + 1))
                ;;
            -*)
                echo "test.sh: unknown focused option: $t" >&2; return 2
                ;;
            *)
                if [ "$i" -eq 0 ] && [ "$have_unit" = "0" ] && list_has "${WORKSPACE_CRATES[*]}" "$t"; then
                    # Documented shorthand: a bare member crate may open the
                    # first unit (e.g. `focused agalma-conductor --test t`).
                    cur_crate="$t"
                    cur_kind=""; cur_tname=""; cur_filter=""; cur_exact=0
                    cur_keys="$pending_keys"; cur_cons="$pending_cons"
                    pending_keys=""; pending_cons=1
                    have_unit=1
                    i=$((i + 1))
                    continue
                fi
                if [ "$have_unit" != "1" ]; then
                    echo "test.sh: filter '$t' appears before any -p <crate>" >&2; return 2
                fi
                if [ -n "$cur_filter" ]; then cur_filter="$cur_filter $t"; else cur_filter="$t"; fi
                i=$((i + 1))
                ;;
        esac
    done
    if [ "$have_unit" = "1" ]; then
        UNIT_CRATE[$UNIT_COUNT]="$cur_crate"
        UNIT_KIND[$UNIT_COUNT]="$cur_kind"
        UNIT_TNAME[$UNIT_COUNT]="$cur_tname"
        UNIT_FILTER[$UNIT_COUNT]="$cur_filter"
        UNIT_EXACT[$UNIT_COUNT]="$cur_exact"
        UNIT_KEYS[$UNIT_COUNT]="$cur_keys"
        UNIT_CONSERVATIVE[$UNIT_COUNT]="$cur_cons"
        UNIT_COUNT=$((UNIT_COUNT + 1))
    fi

    if [ "$UNIT_COUNT" -eq 0 ]; then
        echo "test.sh: focused requires at least one -p <crate> selection" >&2; return 2
    fi

    local u
    u=0
    while [ "$u" -lt "$UNIT_COUNT" ]; do
        local crate="${UNIT_CRATE[$u]}" kind="${UNIT_KIND[$u]}" name="${UNIT_TNAME[$u]}"
        if ! list_has "${WORKSPACE_CRATES[*]}" "$crate"; then
            echo "test.sh: unknown workspace crate '$crate'" >&2; return 2
        fi
        case "$kind" in
            lib)
                if ! crate_has_kind "$crate" lib; then
                    echo "test.sh: crate '$crate' has no library target for --lib" >&2; return 2
                fi
                ;;
            test|bin|example|bench)
                if [ -z "$name" ]; then
                    echo "test.sh: --$kind requires a target name" >&2; return 2
                fi
                if ! target_exists "$crate" "$kind" "$name"; then
                    echo "test.sh: crate '$crate' has no --$kind target '$name'" >&2; return 2
                fi
                ;;
            *)
                echo "test.sh: unit for '$crate' names no target; use --test/--lib/--bin/--example/--bench" >&2; return 2
                ;;
        esac
        u=$((u + 1))
    done
    return 0
}

# Cargo target-selection flag for unit $1 (e.g. "--test sf_cancel").
unit_target_flag() {
    local u="$1"
    case "${UNIT_KIND[$u]}" in
        lib) printf -- '--lib' ;;
        *) printf -- '--%s %s' "${UNIT_KIND[$u]}" "${UNIT_TNAME[$u]}" ;;
    esac
}

# ---------------------------------------------------------------------------
# Workspace metadata
# ---------------------------------------------------------------------------

load_metadata() {
    if ! command -v jq >/dev/null 2>&1; then
        echo "test.sh: jq is required to validate focused selections" >&2
        return 1
    fi
    local -a cargo_flags=()
    [ -n "$CARGO_FLAGS" ] && read -r -a cargo_flags <<< "$CARGO_FLAGS"
    local meta
    if ! meta="$("$CARGO" metadata "${cargo_flags[@]+"${cargo_flags[@]}"}" --no-deps --format-version 1 2>/dev/null)"; then
        echo "test.sh: cargo metadata failed" >&2
        return 1
    fi
    WORKSPACE_CRATES=()
    local crate entry
    while IFS= read -r crate; do
        [ -n "$crate" ] && WORKSPACE_CRATES[${#WORKSPACE_CRATES[@]}]="$crate"
    done <<< "$(printf '%s' "$meta" | jq -r '.packages[].name')"
    TGT_ENTRIES=()
    while IFS= read -r entry; do
        [ -n "$entry" ] && TGT_ENTRIES[${#TGT_ENTRIES[@]}]="$entry"
    done <<< "$(printf '%s' "$meta" | jq -r '.packages[] | .name as $p | .targets[] | [$p, (.kind | join("+")), .name] | join("|")')"
    return 0
}

# ---------------------------------------------------------------------------
# Evidence directory and logging
# ---------------------------------------------------------------------------

# Default evidence root. Evidence must survive worktree removal, so when the
# current checkout is a linked worktree, default to the primary checkout's
# target/test-runs rather than <worktree>/target/test-runs.
default_run_root() {
    local primary top
    primary="$(git worktree list --porcelain 2>/dev/null | sed -n 's/^worktree //p' | head -n 1)"
    top="$(git rev-parse --show-toplevel 2>/dev/null || echo .)"
    if [ -n "$primary" ] && [ "$primary" != "$top" ]; then
        printf '%s\n' "$primary/target/test-runs"
    else
        printf 'target/test-runs\n'
    fi
}

init_run_dir() {
    if [ -z "$RUN_ID" ]; then RUN_ID="$(date -u +%Y%m%dT%H%M%SZ)-$$"; fi
    RUN_DIR="$RUN_ROOT/$BEAD/$RUN_ID"
    mkdir -p "$RUN_DIR"
    printf '%s\n' "$RUN_DIR" > "$RUN_ROOT/$BEAD/.last-run"
}

write_run_env() {
    {
        printf 'scope=%s\n' "$SCOPE"
        printf 'selection=%s\n' "${SEL_ARGS[*]:-}"
        printf 'runner=%s\n' "$0"
        printf 'cwd=%s\n' "$(pwd)"
        printf 'git_rev=%s\n' "$(git rev-parse HEAD 2>/dev/null || echo unknown)"
        printf 'git_branch=%s\n' "$(git rev-parse --abbrev-ref HEAD 2>/dev/null || echo unknown)"
        printf 'cargo=%s\n' "$CARGO"
        printf 'cargo_flags=%s\n' "$CARGO_FLAGS"
        printf 'cargo_incremental=%s\n' "$CARGO_INCREMENTAL"
        printf 'cargo_target_dir=%s\n' "${CARGO_TARGET_DIR:-<default>}"
        printf 'cpu_budget=%s\n' "$CPU_BUDGET"
        printf 'effective_jobs=%s\n' "$EFF_JOBS"
        printf 'effective_threads=%s\n' "$EFF_THREADS"
        printf 'started_utc=%s\n' "$(date -u +%Y-%m-%dT%H:%M:%SZ)"
    } > "$RUN_DIR/run.env"
}

run_logged() {
    local base="$1"; shift
    # Create the parent of the evidence base. Prerequisite builds run before
    # compile_crate creates $RUN_DIR/build, so those bases name a nested
    # directory (e.g. "build/prereq-...") that must exist before writing.
    local dir
    dir="$(dirname "$RUN_DIR/$base")"
    if ! mkdir -p "$dir" || ! printf '%s\n' "$*" > "$RUN_DIR/$base.command"; then
        echo "test.sh: cannot write evidence for $base under $dir" >&2
        return 1
    fi
    local start end rc tee_rc ps
    start="$(now_ms)"
    { "$@"; } 2>&1 | tee "$RUN_DIR/$base.log"
    # Capture both strands of the pipeline in one expansion: a following simple
    # assignment would reset PIPESTATUS before the second index is read.
    ps="${PIPESTATUS[*]}"
    rc="${ps%% *}"
    tee_rc="${ps##* }"
    end="$(now_ms)"
    printf 'exit=%s\n' "$rc" > "$RUN_DIR/$base.status"
    printf 'duration_ms=%s\n' "$((end - start))" > "$RUN_DIR/$base.duration"
    # rc comes from PIPESTATUS[0] (the command); a failure to record the raw log
    # would otherwise be swallowed by the pipeline. Fail the run when evidence
    # cannot be written.
    if [ "$tee_rc" -ne 0 ]; then
        echo "test.sh: failed to record evidence log for $base" >&2
        return 1
    fi
    return "$rc"
}

# ---------------------------------------------------------------------------
# Compilation
# ---------------------------------------------------------------------------

compile_crate() {
    local crate="$1"; shift
    local -a targets=("$@")
    local json="$RUN_DIR/build/$crate.json"
    local log="$RUN_DIR/build/$crate.log"
    mkdir -p "$RUN_DIR/build"
    printf '%s test %s --no-run --message-format=json -p %s %s\n' \
        "$CARGO" "$CARGO_FLAGS" "$crate" "${targets[*]}" > "$json.command"
    echo "[compile] $crate: ${targets[*]}"
    local start end rc
    start="$(now_ms)"
    "$CARGO" test $CARGO_FLAGS --no-run --message-format=json -p "$crate" "${targets[@]}" > "$json" 2> "$log"
    rc=$?
    end="$(now_ms)"
    printf 'exit=%s\n' "$rc" > "$json.status"
    printf 'duration_ms=%s\n' "$((end - start))" > "$json.duration"
    if [ "$rc" -ne 0 ]; then cat "$log" >&2; return "$rc"; fi
    local kind name exe manifest
    while IFS=$'\t' read -r kind name exe manifest; do
        [ -z "$exe" ] && continue
        EXE_CRATE[${#EXE_CRATE[@]}]="$crate"
        EXE_KIND[${#EXE_KIND[@]}]="$kind"
        EXE_NAME[${#EXE_NAME[@]}]="$name"
        EXE_PATH[${#EXE_PATH[@]}]="$exe"
        EXE_MANIFEST[${#EXE_MANIFEST[@]}]="$manifest"
    done < <(jq -r 'select(.reason=="compiler-artifact" and .executable != null) | [(.target.kind | join("+")), .target.name, .executable, .manifest_path] | @tsv' "$json")
    return 0
}

# Locate the compiled executable for unit $1; sets EXE_FOUND_* globals.
find_exe_for_unit() {
    local u="$1"
    local crate="${UNIT_CRATE[$u]}" kind="${UNIT_KIND[$u]}" name="${UNIT_TNAME[$u]}"
    local k
    k=0
    while [ "$k" -lt "${#EXE_PATH[@]}" ]; do
        local hit=0
        if [ "${EXE_CRATE[$k]}" = "$crate" ]; then
            case ";${EXE_KIND[$k]};" in *";$kind;"*) hit=1 ;; esac
            if [ "$hit" = "1" ] && [ "$kind" != "lib" ] && [ "${EXE_NAME[$k]}" != "$name" ]; then hit=0; fi
        fi
        if [ "$hit" = "1" ]; then
            EXE_FOUND_KIND="${EXE_KIND[$k]}"; EXE_FOUND_NAME="${EXE_NAME[$k]}"
            EXE_FOUND_PATH="${EXE_PATH[$k]}"; EXE_FOUND_MANIFEST="${EXE_MANIFEST[$k]}"
            return 0
        fi
        k=$((k + 1))
    done
    return 1
}

# ---------------------------------------------------------------------------
# Focused planning
# ---------------------------------------------------------------------------

collect_prerequisites() {
    PREREQ_CRATES=()
    PREREQ_BINS=0
    local i=0 n=${#SEL_ARGS[@]}
    while [ "$i" -lt "$n" ]; do
        case "${SEL_ARGS[$i]}" in
            --prereq) PREREQ_CRATES[${#PREREQ_CRATES[@]}]="${SEL_ARGS[$((i + 1))]}"; i=$((i + 2)) ;;
            --prereq-bins) PREREQ_BINS=1; i=$((i + 1)) ;;
            *) i=$((i + 1)) ;;
        esac
    done
}

build_prerequisites() {
    local rc=0 c
    if [ "$PREREQ_BINS" = "1" ]; then
        echo "[compile] prerequisites: workspace bins"
        run_logged "build/prereq-workspace-bins" "$CARGO" build $CARGO_FLAGS --workspace --bins || rc=1
    fi
    for c in ${PREREQ_CRATES[@]+"${PREREQ_CRATES[@]}"}; do
        echo "[compile] prerequisites: -p $c --bins"
        run_logged "build/prereq-$c-bins" "$CARGO" build $CARGO_FLAGS -p "$c" --bins || rc=1
    done
    return "$rc"
}

selected_crates() {
    local crates="" u
    for u in $(seq 0 $((UNIT_COUNT - 1))); do
        if ! list_has "$crates" "${UNIT_CRATE[$u]}"; then crates="$crates ${UNIT_CRATE[$u]}"; fi
    done
    printf '%s' "$crates"
}

# Record a unit that cannot run as a failed job so the aggregate fails and the
# evidence names the reason.
report_unit_failure() {
    local u="$1" label="$2" reason="$3"
    JOB_LABEL[$JOB_COUNT]="$label"
    JOB_CWD[$JOB_COUNT]="$(pwd)"
    JOB_EXE[$JOB_COUNT]="<unavailable>"
    JOB_ARGV[$JOB_COUNT]=""
    JOB_KEYS[$JOB_COUNT]="${UNIT_KEYS[$u]}"
    JOB_CONSERVATIVE[$JOB_COUNT]="${UNIT_CONSERVATIVE[$u]}"
    JOB_LOG[$JOB_COUNT]="$RUN_DIR/jobs/$(printf '%02d' "$JOB_COUNT")-$label.log"
    printf '%s: %s\n' "$label" "$reason" > "${JOB_LOG[$JOB_COUNT]}"
    JOB_DISCOVERED[$JOB_COUNT]=0
    JOB_UNRUNNABLE[$JOB_COUNT]=1
    JOB_RC[$JOB_COUNT]=1
    JOB_COUNT=$((JOB_COUNT + 1))
}

build_jobs() {
    local u
    JOB_COUNT=0
    JOB_LABEL=(); JOB_CWD=(); JOB_EXE=(); JOB_ARGV=(); JOB_KEYS=()
    JOB_CONSERVATIVE=(); JOB_LOG=(); JOB_RC=(); JOB_START_MS=(); JOB_END_MS=()
    JOB_OVERLAP=(); JOB_DISCOVERED=(); JOB_PASSED=(); JOB_FAILED=(); JOB_IGNORED=()
    JOB_OK=(); JOB_PID=(); JOB_UNRUNNABLE=()
    DISCOVERED_CASES=0
    mkdir -p "$RUN_DIR/jobs"

    u=0
    while [ "$u" -lt "$UNIT_COUNT" ]; do
        local crate="${UNIT_CRATE[$u]}" kind="${UNIT_KIND[$u]}" name="${UNIT_TNAME[$u]}"
        local label="$crate::${name:-lib}"
        [ "$kind" = "lib" ] && label="$crate::lib"

        if ! find_exe_for_unit "$u"; then
            echo "[discover] $label: compiled executable not found" >&2
            report_unit_failure "$u" "$label" "no compiled executable"
            u=$((u + 1)); continue
        fi
        local exe="$EXE_FOUND_PATH" manifest="$EXE_FOUND_MANIFEST"
        local cwd; cwd="$(dirname "$manifest")"
        local filter="${UNIT_FILTER[$u]}"

        # Discover matching required cases before execution. Apply --exact for
        # units that carry it, so `discovered` reflects the cases the run will
        # actually execute rather than every substring match.
        local -a dargv=()
        [ -n "$filter" ] && read -r -a dargv <<< "$filter"
        [ "${UNIT_EXACT[$u]}" = "1" ] && dargv[${#dargv[@]}]="--exact"
        local list_out count
        list_out="$( cd "$cwd" && "$exe" ${dargv[@]+"${dargv[@]}"} --list 2>&1 )"
        count="$(printf '%s\n' "$list_out" | grep -cE ': test$')"
        if [ "${count:-0}" -eq 0 ]; then
            echo "[discover] $label: filter matched 0 required cases" >&2
            report_unit_failure "$u" "$label" "filter matched zero required cases"
            u=$((u + 1)); continue
        fi

        # Assemble argv: filter tokens, optional --exact, --test-threads.
        local argbuf="" ftok
        local -a fargv=()
        [ -n "$filter" ] && read -r -a fargv <<< "$filter"
        for ftok in ${fargv[@]+"${fargv[@]}"}; do argbuf="$argbuf$ftok
"; done
        [ "${UNIT_EXACT[$u]}" = "1" ] && argbuf="${argbuf}--exact
"
        argbuf="${argbuf}--test-threads=$EFF_THREADS
"

        JOB_LABEL[$JOB_COUNT]="$label"
        JOB_CWD[$JOB_COUNT]="$cwd"
        JOB_EXE[$JOB_COUNT]="$exe"
        JOB_ARGV[$JOB_COUNT]="$argbuf"
        JOB_KEYS[$JOB_COUNT]="${UNIT_KEYS[$u]}"
        JOB_CONSERVATIVE[$JOB_COUNT]="${UNIT_CONSERVATIVE[$u]}"
        JOB_LOG[$JOB_COUNT]="$RUN_DIR/jobs/$(printf '%02d' "$JOB_COUNT")-$label.log"
        JOB_DISCOVERED[$JOB_COUNT]="$count"
        JOB_COUNT=$((JOB_COUNT + 1))
        DISCOVERED_CASES=$((DISCOVERED_CASES + count))
        u=$((u + 1))
    done
}

# ---------------------------------------------------------------------------
# Bounded worker pool
# ---------------------------------------------------------------------------

on_signal() {
    INTERRUPTED=1
    echo "[run] interrupt received; terminating ${#POOL_RUNNING[@]} running job(s)" >&2
    local k
    for k in ${POOL_RUNNING[@]+"${POOL_RUNNING[@]}"}; do
        kill_tree "${JOB_PID[$k]}" TERM
    done
    sleep 0.3
    for k in ${POOL_RUNNING[@]+"${POOL_RUNNING[@]}"}; do
        kill_tree "${JOB_PID[$k]}" KILL
        wait "${JOB_PID[$k]}" 2>/dev/null || true
    done
    trap - INT TERM
}

# Kill a process and its descendants.
kill_tree() {
    local pid="$1" sig="$2" child
    for child in $(pgrep -P "$pid" 2>/dev/null || true); do
        kill_tree "$child" "$sig"
    done
    kill -"$sig" "$pid" 2>/dev/null || true
}

launch_job() {
    local j="$1"
    local overlap="" k
    for k in ${POOL_RUNNING[@]+"${POOL_RUNNING[@]}"}; do overlap="$overlap$k
"; done
    JOB_OVERLAP[$j]="$overlap"
    JOB_START_MS[$j]="$(now_ms)"
    rm -f "$JOB_STATUS_DIR/$j.status"
    (
        cd "${JOB_CWD[$j]}" 2>/dev/null || cd "$(pwd)"
        local -a argv=()
        local line
        while IFS= read -r line; do
            [ -n "$line" ] && argv[${#argv[@]}]="$line"
        done <<< "${JOB_ARGV[$j]}"
        "${JOB_EXE[$j]}" ${argv[@]+"${argv[@]}"} > "${JOB_LOG[$j]}" 2>&1
        rc=$?
        printf '%s\n' "$rc" > "$JOB_STATUS_DIR/$j.status"
        exit "$rc"
    ) &
    JOB_PID[$j]=$!
}

# Run all JOB_* entries through a bounded pool honouring EFF_JOBS and job
# conflicts. Sets JOB_RC/JOB_START_MS/JOB_END_MS/JOB_OVERLAP and
# PEAK_CONCURRENCY. The pool is a pure scheduler: tests may drive it with fake
# job commands.
run_pool() {
    POOL_RUNNING=()
    POOL_STATE=()
    local j total=0 peak=0
    j=0
    while [ "$j" -lt "$JOB_COUNT" ]; do
        if [ "${JOB_UNRUNNABLE[$j]:-0}" = "1" ]; then
            POOL_STATE[$j]=2; JOB_RC[$j]=1; JOB_START_MS[$j]=0; JOB_END_MS[$j]=0
        else
            POOL_STATE[$j]=0; JOB_RC[$j]=0
        fi
        j=$((j + 1))
    done

    trap 'on_signal' INT
    trap 'on_signal' TERM

    while :; do
        # Fill free slots with non-conflicting pending jobs.
        while [ "${#POOL_RUNNING[@]}" -lt "$EFF_JOBS" ]; do
            local pick="" k
            j=0
            while [ "$j" -lt "$JOB_COUNT" ]; do
                if [ "${POOL_STATE[$j]}" != "0" ]; then j=$((j + 1)); continue; fi
                local conflict=0
                for k in ${POOL_RUNNING[@]+"${POOL_RUNNING[@]}"}; do
                    if jobs_conflict "$j" "$k"; then conflict=1; break; fi
                done
                if [ "$conflict" = "0" ]; then pick="$j"; break; fi
                j=$((j + 1))
            done
            [ -z "$pick" ] && break
            launch_job "$pick"
            POOL_STATE[$pick]=1
            POOL_RUNNING[${#POOL_RUNNING[@]}]="$pick"
            total=$((total + 1))
            [ "${#POOL_RUNNING[@]}" -gt "$peak" ] && peak="${#POOL_RUNNING[@]}"
        done

        # Reap finished jobs (their status file is written before exit).
        local still=() done_any=0
        for k in ${POOL_RUNNING[@]+"${POOL_RUNNING[@]}"}; do
            if [ -f "$JOB_STATUS_DIR/$k.status" ]; then
                wait "${JOB_PID[$k]}" 2>/dev/null
                JOB_RC[$k]=$?
                JOB_END_MS[$k]="$(now_ms)"
                POOL_STATE[$k]=2
                done_any=1
            else
                still[${#still[@]}]="$k"
            fi
        done
        POOL_RUNNING=(${still[@]+"${still[@]}"})

        [ "$INTERRUPTED" = "1" ] && break

        local remaining=0
        j=0
        while [ "$j" -lt "$JOB_COUNT" ]; do
            if [ "${POOL_STATE[$j]}" = "0" ] || [ "${POOL_STATE[$j]}" = "1" ]; then remaining=$((remaining + 1)); fi
            j=$((j + 1))
        done
        if [ "$remaining" -eq 0 ]; then break; fi
        if [ "${#POOL_RUNNING[@]}" -gt 0 ]; then
            sleep 0.05
        elif [ "$done_any" = "0" ]; then
            sleep 0.02
        fi
    done

    PEAK_CONCURRENCY="$peak"
    if [ "$INTERRUPTED" = "1" ]; then
        local k
        for k in ${POOL_RUNNING[@]+"${POOL_RUNNING[@]}"}; do
            JOB_RC[$k]=130
            [ -z "${JOB_END_MS[$k]:-}" ] && JOB_END_MS[$k]="$(now_ms)"
        done
    fi
    trap - INT TERM
}

# ---------------------------------------------------------------------------
# Result parsing and aggregation
# ---------------------------------------------------------------------------

# Parse a libtest log; sets PARSED_PASSED/PARSED_FAILED/PARSED_IGNORED. Returns
# 0 when a final `test result:` line was found. Only the last such line counts,
# so child harness output cannot inflate the top-level totals.
parse_result_log() {
    local log="$1" line parsed
    line="$(grep -E '^test result: ' "$log" 2>/dev/null | tail -n 1)"
    [ -z "$line" ] && return 1
    parsed="$(printf '%s\n' "$line" | sed -E 's/^test result: (ok|FAILED)\. ([0-9]+) passed; ([0-9]+) failed; ([0-9]+) ignored.*$/\2 \3 \4/')"
    PARSED_PASSED="${parsed%% *}"
    local rest="${parsed#* }"
    PARSED_FAILED="${rest%% *}"
    PARSED_IGNORED="${rest##* }"
    return 0
}

parse_job_logs() {
    local j
    j=0
    while [ "$j" -lt "$JOB_COUNT" ]; do
        JOB_PASSED[$j]=0; JOB_FAILED[$j]=0; JOB_IGNORED[$j]=0
        if [ "${JOB_UNRUNNABLE[$j]:-0}" = "1" ]; then
            JOB_RC[$j]=1
            j=$((j + 1)); continue
        fi
        if [ -f "${JOB_LOG[$j]}" ] && parse_result_log "${JOB_LOG[$j]}"; then
            JOB_PASSED[$j]="$PARSED_PASSED"
            JOB_FAILED[$j]="$PARSED_FAILED"
            JOB_IGNORED[$j]="$PARSED_IGNORED"
        fi
        j=$((j + 1))
    done
}

# Combine per-job outcomes into the run aggregate. Sets AGG_STATUS/PASSED/
# FAILED/IGNORED. A job fails when it exited non-zero, executed zero cases, or
# matched only ignored cases; any such failure fails the run.
aggregate_results() {
    local j total_p=0 total_f=0 total_i=0 failed=0
    j=0
    while [ "$j" -lt "$JOB_COUNT" ]; do
        local p="${JOB_PASSED[$j]:-0}" f="${JOB_FAILED[$j]:-0}" ig="${JOB_IGNORED[$j]:-0}" rc="${JOB_RC[$j]:-1}"
        total_p=$((total_p + p)); total_f=$((total_f + f)); total_i=$((total_i + ig))
        if [ "$rc" != "0" ] || [ "$p" = "0" ]; then
            JOB_OK[$j]=0; failed=1
        else
            JOB_OK[$j]=1
        fi
        j=$((j + 1))
    done
    AGG_PASSED="$total_p"; AGG_FAILED="$total_f"; AGG_IGNORED="$total_i"
    if [ "$INTERRUPTED" = "1" ]; then AGG_STATUS="incomplete"; return 0; fi
    if [ "$failed" = "1" ]; then AGG_STATUS="failed"; else AGG_STATUS="passed"; fi
}

# ---------------------------------------------------------------------------
# Summary
# ---------------------------------------------------------------------------

write_summary() {
    local status="${AGG_STATUS:-failed}"
    [ "$INTERRUPTED" = "1" ] && status="incomplete"
    local compile_ms=0 f exec_ms=0 j
    if [ -d "$RUN_DIR/build" ]; then
        for f in "$RUN_DIR"/build/*.duration; do
            [ -f "$f" ] || continue
            local d; d="$(sed -n 's/^duration_ms=//p' "$f")"
            [ -n "$d" ] && compile_ms=$((compile_ms + d))
        done
    fi
    j=0
    while [ "$j" -lt "$JOB_COUNT" ]; do
        local s="${JOB_START_MS[$j]:-0}" e="${JOB_END_MS[$j]:-0}"
        [ "$e" -gt "$s" ] && exec_ms=$((exec_ms + (e - s)))
        j=$((j + 1))
    done

    {
        printf '{\n'
        printf '  "scope": "%s",\n' "$(json_escape "$SCOPE")"
        printf '  "status": "%s",\n' "$status"
        printf '  "bead": "%s",\n' "$(json_escape "$BEAD")"
        printf '  "run_id": "%s",\n' "$(json_escape "$RUN_ID")"
        printf '  "selection": "%s",\n' "$(json_escape "${SEL_ARGS[*]:-}")"
        printf '  "jobs": %s,\n' "$EFF_JOBS"
        printf '  "threads": %s,\n' "$EFF_THREADS"
        printf '  "cpu_budget": %s,\n' "$CPU_BUDGET"
        printf '  "peak_concurrency": %s,\n' "$PEAK_CONCURRENCY"
        printf '  "compile_ms_sum": %s,\n' "$compile_ms"
        printf '  "exec_ms_sum": %s,\n' "$exec_ms"
        printf '  "passed": %s,\n' "${AGG_PASSED:-0}"
        printf '  "failed": %s,\n' "${AGG_FAILED:-0}"
        printf '  "ignored": %s,\n' "${AGG_IGNORED:-0}"
        printf '  "targets": [\n'
        j=0
        while [ "$j" -lt "$JOB_COUNT" ]; do
            local comma=","
            [ "$j" -eq $((JOB_COUNT - 1)) ] && comma=""
            printf '    {"label":"%s","group":"%s","conservative":%s,"discovered":%s,"rc":%s,"passed":%s,"failed":%s,"ignored":%s,"ok":%s,"start_ms":%s,"end_ms":%s,"log":"%s"}%s\n' \
                "$(json_escape "${JOB_LABEL[$j]}")" \
                "$(json_escape "${JOB_KEYS[$j]}")" \
                "${JOB_CONSERVATIVE[$j]:-0}" \
                "${JOB_DISCOVERED[$j]:-0}" "${JOB_RC[$j]:-1}" \
                "${JOB_PASSED[$j]:-0}" "${JOB_FAILED[$j]:-0}" "${JOB_IGNORED[$j]:-0}" \
                "${JOB_OK[$j]:-0}" "${JOB_START_MS[$j]:-0}" "${JOB_END_MS[$j]:-0}" \
                "$(json_escape "$(basename "${JOB_LOG[$j]}")")" "$comma"
            j=$((j + 1))
        done
        printf '  ]\n'
        printf '}\n'
    } > "$RUN_DIR/summary.json"

    {
        echo "# test run $RUN_ID ($SCOPE)"
        echo
        echo "- bead: $BEAD"
        echo "- status: $status"
        echo "- selection: ${SEL_ARGS[*]:-}"
        echo "- budget: cpu=$CPU_BUDGET jobs=$EFF_JOBS threads=$EFF_THREADS"
        echo "- peak concurrent binaries: $PEAK_CONCURRENCY"
        echo "- compile duration (sum, ms): $compile_ms"
        echo "- exec duration (sum, ms): $exec_ms"
        echo "- cases: passed=${AGG_PASSED:-0} failed=${AGG_FAILED:-0} ignored=${AGG_IGNORED:-0}"
        echo
        echo "| target | group | isolation | discovered | rc | passed | failed | ignored | ms | log |"
        echo "|---|---|---|---|---|---|---|---|---|---|"
        j=0
        while [ "$j" -lt "$JOB_COUNT" ]; do
            local iso="conservative"
            if [ "${JOB_CONSERVATIVE[$j]}" = "0" ]; then
                if [ -z "${JOB_KEYS[$j]}" ]; then iso="isolated"; else iso="${JOB_KEYS[$j]}"; fi
            fi
            printf '| %s | %s | %s | %s | %s | %s | %s | %s | %s | %s |\n' \
                "${JOB_LABEL[$j]}" "${JOB_KEYS[$j]:--}" "$iso" "${JOB_DISCOVERED[$j]:-0}" \
                "${JOB_RC[$j]:-?}" "${JOB_PASSED[$j]:-0}" "${JOB_FAILED[$j]:-0}" "${JOB_IGNORED[$j]:-0}" \
                "$(( ${JOB_END_MS[$j]:-0} - ${JOB_START_MS[$j]:-0} ))" "$(basename "${JOB_LOG[$j]}")"
            j=$((j + 1))
        done
    } > "$RUN_DIR/summary.txt"

    echo "[summary] status=$status peak_concurrency=$PEAK_CONCURRENCY passed=${AGG_PASSED:-0} failed=${AGG_FAILED:-0} ignored=${AGG_IGNORED:-0}"
    echo "[summary] evidence: $RUN_DIR/summary.json"
}

# ---------------------------------------------------------------------------
# Scope drivers
# ---------------------------------------------------------------------------

run_focused() {
    collect_prerequisites
    if ! load_metadata; then return 2; fi
    if ! parse_selection; then return 2; fi

    if [ "$PRINT_ONLY" = "1" ]; then print_focused_plan; return 0; fi

    init_run_dir
    write_run_env
    build_plan_file
    freeze_build_inputs
    echo "[scope] focused"
    local budget_note=""
    if [ $((EFF_JOBS * EFF_THREADS)) -gt "$CPU_BUDGET" ]; then
        budget_note=" [explicit override exceeds cpu budget]"
    fi
    echo "[budget] cpu=$CPU_BUDGET jobs=$EFF_JOBS threads=$EFF_THREADS (max concurrent case threads=$((EFF_JOBS * EFF_THREADS)))$budget_note"
    echo "[selection] ${SEL_ARGS[*]}"

    if ! build_prerequisites; then
        echo "test.sh: prerequisite build failed" >&2
        AGG_STATUS="failed"; write_summary; return 1
    fi

    # Compile each distinct crate once.
    local crate rc=0 u
    for crate in $(selected_crates); do
        local -a flat=() targets=() ftok
        u=0
        while [ "$u" -lt "$UNIT_COUNT" ]; do
            if [ "${UNIT_CRATE[$u]}" = "$crate" ]; then
                targets[${#targets[@]}]="$(unit_target_flag "$u")"
            fi
            u=$((u + 1))
        done
        for ftok in "${targets[@]}"; do
            local -a one=()
            read -r -a one <<< "$ftok"
            flat=("${flat[@]}" ${one[@]+"${one[@]}"})
        done
        compile_crate "$crate" ${flat[@]+"${flat[@]}"} || { echo "test.sh: compilation failed for $crate" >&2; rc=1; }
    done
    if [ "$rc" -ne 0 ]; then AGG_STATUS="failed"; write_summary; return 1; fi

    build_jobs
    JOB_STATUS_DIR="$RUN_DIR/jobs"
    if [ "$JOB_COUNT" -gt 0 ]; then run_pool; fi
    parse_job_logs
    aggregate_results
    write_summary
    case "$AGG_STATUS" in
        passed) return 0 ;;
        incomplete) return 130 ;;
        *) return 1 ;;
    esac
}

print_focused_plan() {
    local crate u
    for crate in $(selected_crates); do
        local -a flat=() one=()
        u=0
        while [ "$u" -lt "$UNIT_COUNT" ]; do
            if [ "${UNIT_CRATE[$u]}" = "$crate" ]; then
                read -r -a one <<< "$(unit_target_flag "$u")"
                flat=("${flat[@]}" ${one[@]+"${one[@]}"})
            fi
            u=$((u + 1))
        done
        echo "[print] compile: $CARGO test $CARGO_FLAGS --no-run --message-format=json -p $crate ${flat[*]}"
    done
    u=0
    while [ "$u" -lt "$UNIT_COUNT" ]; do
        local label="${UNIT_CRATE[$u]}::${UNIT_KIND[$u]}"
        [ "${UNIT_KIND[$u]}" != "lib" ] && label="${UNIT_CRATE[$u]}::${UNIT_TNAME[$u]}"
        local iso="conservative"
        if [ "${UNIT_CONSERVATIVE[$u]}" = "0" ]; then
            if [ -z "${UNIT_KEYS[$u]}" ]; then iso="isolated"; else iso="${UNIT_KEYS[$u]}"; fi
        fi
        echo "[print] run: $label filter='${UNIT_FILTER[$u]}' exact=${UNIT_EXACT[$u]} isolation=$iso"
        u=$((u + 1))
    done
}

build_plan_file() {
    {
        echo "# focused plan"
        local u=0
        while [ "$u" -lt "$UNIT_COUNT" ]; do
            local iso
            if [ "${UNIT_CONSERVATIVE[$u]}" = "1" ]; then iso="conservative-unknown"
            elif [ -z "${UNIT_KEYS[$u]}" ]; then iso="isolated"
            else iso="group${UNIT_KEYS[$u]}"; fi
            printf 'unit=%s crate=%s target=%s%s filter=%s exact=%s isolation=%s\n' \
                "$u" "${UNIT_CRATE[$u]}" "${UNIT_KIND[$u]}" \
                "${UNIT_TNAME[$u]:+ ${UNIT_TNAME[$u]}}" \
                "${UNIT_FILTER[$u]:-<all>}" "${UNIT_EXACT[$u]}" "$iso"
            u=$((u + 1))
        done
    } > "$RUN_DIR/plan.txt"
}

freeze_build_inputs() {
    git rev-parse HEAD > "$RUN_DIR/build-inputs.txt" 2>/dev/null || echo unknown > "$RUN_DIR/build-inputs.txt"
}

# ---------------------------------------------------------------------------
# Peers and full scopes (retain the documented contracts)
# ---------------------------------------------------------------------------

peers_commands() {
    cat <<'EOF'
cargo build --offline --workspace --bins
cargo test --offline -p agalma-conductor --test ledger_process --test workspace_process --test taskqueue_process --test execution_process live_process_smoke -- --nocapture --test-threads=1
cargo test --offline -p agalma-conductor -- --test-threads=1
EOF
}

full_commands() {
    echo "$CARGO build $CARGO_FLAGS --workspace --bins"
    echo "$CARGO test $CARGO_FLAGS --workspace"
}

run_peers() {
    if [ "$PRINT_ONLY" = "1" ]; then peers_commands; return 0; fi
    init_run_dir
    write_run_env
    echo "[scope] peers (local process smokes on the default Connect wire)"
    unset AGALMA_IPC_WIRE AGALMA_HARNESS_LIVE AGALMA_HARNESS_TRANSPORT
    unset AGALMA_LEDGER_TRANSPORT AGALMA_WORKSPACE_TRANSPORT
    unset AGALMA_TASKQUEUE_TRANSPORT AGALMA_EXECUTION_TRANSPORT

    if ! run_logged peers-build "$CARGO" build --offline --workspace --bins; then
        echo "test.sh: peer build failed" >&2; return 1
    fi
    local target_dir="${CARGO_TARGET_DIR:-target}"
    if [ -n "${CARGO_BUILD_TARGET:-}" ]; then target_dir="$target_dir/$CARGO_BUILD_TARGET"; fi

    local component binary output
    for component in ledger workspace taskqueue kernel; do
        binary="$target_dir/debug/agalma-$component-server"
        if [ ! -x "$binary" ]; then echo "test.sh: missing binary $binary" >&2; return 1; fi
        shasum -a 256 "$binary"
        if output="$("$binary" 2>&1)"; then
            echo "test.sh: expected argument-parser rejection from $binary" >&2; return 1
        fi
        if [[ "$output" != *'missing required --socket <path>'* ]]; then
            echo "test.sh: preflight failed for $binary: $output" >&2; return 1
        fi
        echo "executable preflight passed: $component"
    done

    if ! run_logged peers-smoke "$CARGO" test --offline -p agalma-conductor \
        --test ledger_process --test workspace_process --test taskqueue_process \
        --test execution_process \
        live_process_smoke -- --nocapture --test-threads=1; then
        echo "test.sh: peer smoke failed" >&2; return 1
    fi
    local smoke_output
    smoke_output="$(cat "$RUN_DIR/peers-smoke.log")"
    if [ "$(printf '%s\n' "$smoke_output" | rg -c 'test result: ok\. 1 passed; 0 failed; 0 ignored;')" != 4 ]; then
        echo "test.sh: required smoke evidence must include four executed, passing cases" >&2; return 1
    fi
    if [[ "$smoke_output" == *'skip:'* ]]; then
        echo "test.sh: required smoke evidence must not contain skipped cases" >&2; return 1
    fi

    if ! run_logged peers-conductor "$CARGO" test --offline -p agalma-conductor -- --test-threads=1; then
        echo "test.sh: peer conductor suite failed" >&2; return 1
    fi
    echo "[summary] peers passed"
    return 0
}

run_full() {
    if [ "$PRINT_ONLY" = "1" ]; then full_commands; return 0; fi
    init_run_dir
    write_run_env
    echo "[scope] full (workspace tests)"
    if ! run_logged full-build "$CARGO" build $CARGO_FLAGS --workspace --bins; then
        echo "test.sh: full build failed" >&2; return 1
    fi
    if ! run_logged full "$CARGO" test $CARGO_FLAGS --workspace; then
        echo "test.sh: full suite failed" >&2; return 1
    fi
    echo "[summary] full passed"
    return 0
}

# ---------------------------------------------------------------------------
# Budget and entry point
# ---------------------------------------------------------------------------

compute_budget() {
    CPU_BUDGET="$(getconf _NPROCESSORS_ONLN 2>/dev/null || sysctl -n hw.ncpu 2>/dev/null || echo 1)"
    case "$CPU_BUDGET" in ''|*[!0-9]*) CPU_BUDGET=1 ;; esac
    [ "$CPU_BUDGET" -lt 1 ] && CPU_BUDGET=1
    EFF_JOBS="$JOBS_REQUESTED"
    case "$EFF_JOBS" in ''|*[!0-9]*) EFF_JOBS=1 ;; esac
    [ "$EFF_JOBS" -lt 1 ] && EFF_JOBS=1
    [ "$EFF_JOBS" -gt "$CPU_BUDGET" ] && EFF_JOBS="$CPU_BUDGET"
    if [ -n "$THREADS_REQUESTED" ]; then
        EFF_THREADS="$THREADS_REQUESTED"
        case "$EFF_THREADS" in ''|*[!0-9]*) EFF_THREADS=1 ;; esac
    else
        EFF_THREADS=$((CPU_BUDGET / EFF_JOBS))
    fi
    [ "$EFF_THREADS" -lt 1 ] && EFF_THREADS=1
}

usage() {
    cat >&2 <<'EOF'
Usage: bash scripts/test.sh <focused|peers|full> [selection]

Scopes:
  focused  Explicit crate + target/module selection. Requires at least one unit:
             -p <crate> [--isolated|--group <name>] (--test <name>|--lib|--bin <name>|--example <name>|--bench <name>) [filter...] [--exact]
           Isolation markers are per unit and default to conservative:
             --isolated        known-isolated; may overlap other units
             --group <name>    named exclusive resource; same group serializes
             (no marker)       unknown isolation; serializes with every unit
           Prerequisites: --prereq <crate> (its bins) or --prereq-bins.
           Does not establish workspace acceptance or unselected boundaries.
  peers    Four local process smokes on the default Connect wire, plus the
           retained conductor suite. Excludes paid-provider checks.
  full     Workspace tests with binary prerequisites. Excludes ignored
           live-provider checks. Run once, not per implementation.

No scope prints this usage without building or running tests.

Prerequisites: `jq` (selection validation and compile-output parsing) and `rg`
(peer-scope evidence checks) must be on PATH.

Environment: BEAD (evidence label), RUN_ID, RUN_ROOT, JOBS (default 2),
THREADS (per-binary case threads), CARGO, CARGO_FLAGS, CARGO_TARGET_DIR.
EOF
}

main() {
    if [ "$#" -eq 0 ]; then usage; exit 2; fi
    SCOPE="$1"; shift
    case "$SCOPE" in
        help|-h|--help) usage; exit 0 ;;
        focused|peers|full) ;;
        *) echo "test.sh: unknown scope '$SCOPE'" >&2; usage; exit 2 ;;
    esac
    # Diagnostic: print the commands a scope would run without executing them.
    if [ "${1:-}" = "--print" ]; then PRINT_ONLY=1; shift; fi
    SEL_ARGS=("$@")

    local here
    here="$(cd -- "$(dirname -- "${BASH_SOURCE[0]}")/.." && pwd)"
    cd "$here" || exit 1

    [ -z "$RUN_ROOT" ] && RUN_ROOT="$(default_run_root)"

    compute_budget

    case "$SCOPE" in
        focused) run_focused; exit $? ;;
        peers)   run_peers; exit $? ;;
        full)    run_full; exit $? ;;
    esac
}

if [ "${BASH_SOURCE[0]}" = "$0" ]; then
    main "$@"
fi
