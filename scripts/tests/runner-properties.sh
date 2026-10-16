#!/usr/bin/env bash
# Bounded property tests for the focused runner's own invariants
# (scripts/test.sh): selection parsing/validation, worker-pool concurrency and
# exclusion bounds, and outcome aggregation.
#
# These are the native tests for a bash tool: they source the script under test
# and drive its pure functions with deterministically generated inputs. No
# external test framework or dependency is introduced. Each property defines an
# invariant independently of the implementation. Generation is deterministic
# (a fixed-seed LCG) and the seed/case budget is recorded in the run receipt.
#
# Evidence lands under $RUN_ROOT/$BEAD/$RUN_ID/properties/.

set -o pipefail

repo_root="$(cd -- "$(dirname -- "${BASH_SOURCE[0]}")/../.." && pwd)"
cd "$repo_root" || exit 1

# shellcheck source=../test.sh
source "$repo_root/scripts/test.sh"

BEAD="${BEAD:-agalma-i15}"
RUN_ROOT="${RUN_ROOT:-$(default_run_root)}"
RUN_ID="${RUN_ID:-props-$(date -u +%Y%m%dT%H%M%SZ)-$$}"
OUT="$RUN_ROOT/$BEAD/$RUN_ID/properties"
mkdir -p "$OUT"

SEED="${PROP_SEED:-20261015}"
STATE="$SEED"
lcg() {
    STATE=$(( (STATE * 1103515245 + 12345) % 2147483648 ))
    printf '%s' "$STATE"
}
rand_below() { printf '%s' "$(( $(lcg) % $1 ))"; }

PASS=0
FAIL=0
FAILED=""

report() { printf '%s\n' "$1" >> "$OUT/properties.log"; }

check() {
    local desc="$1"; shift
    if "$@"; then
        PASS=$((PASS + 1))
    else
        FAIL=$((FAIL + 1))
        FAILED="$FAILED
  - $desc"
        report "FAIL: $desc"
        echo "FAIL: $desc" >&2
    fi
}

eq() { [ "$1" = "$2" ] || { echo "  expected [$1] got [$2]" >&2; return 1; }; }
neq() { [ "$1" != "$2" ]; }
nonzero() { "$@" >/dev/null 2>&1; [ $? -ne 0 ]; }

# ---------------------------------------------------------------------------
# Synthetic workspace for selection parsing.
# ---------------------------------------------------------------------------
WORKSPACE_CRATES=(alpha beta gamma)
TGT_ENTRIES=(
    "alpha|lib|alpha"
    "alpha|test|t_alpha"
    "alpha|test|t_alpha2"
    "beta|test|t_beta"
    "beta|bin|b_beta"
    "gamma|example|e_gamma"
    "gamma|bench|bn_gamma"
)

gen_crate=()
gen_kind=()
gen_name=()
gen_filter=()
gen_keys=()
gen_cons=()

# Build a random focused selection from valid (crate,kind,name) pairs plus a
# per-unit filter and isolation marker. Deterministic given the shared LCG.
generate_selection() { # $1 = unit count
    local units="$1" u
    gen_crate=(); gen_kind=(); gen_name=(); gen_filter=(); gen_keys=(); gen_cons=()
    SEL_ARGS=()
    u=0
    while [ "$u" -lt "$units" ]; do
        local choice crate kind name marker r filt
        choice="$(rand_below 6)"
        case "$choice" in
            0) crate=alpha; kind=test; name=t_alpha ;;
            1) crate=alpha; kind=test; name=t_alpha2 ;;
            2) crate=beta;  kind=test; name=t_beta ;;
            3) crate=beta;  kind=bin;  name=b_beta ;;
            4) crate=gamma; kind=example; name=e_gamma ;;
            *) crate=gamma; kind=bench; name=bn_gamma ;;
        esac
        SEL_ARGS[${#SEL_ARGS[@]}]="-p"
        SEL_ARGS[${#SEL_ARGS[@]}]="$crate"
        marker="$(rand_below 3)"
        case "$marker" in
            0) SEL_ARGS[${#SEL_ARGS[@]}]="--isolated"; gen_keys[$u]=""; gen_cons[$u]=0 ;;
            1) SEL_ARGS[${#SEL_ARGS[@]}]="--group"; SEL_ARGS[${#SEL_ARGS[@]}]="res$u"
               gen_keys[$u]=";group:res$u"; gen_cons[$u]=0 ;;
            *) gen_keys[$u]=""; gen_cons[$u]=1 ;;
        esac
        SEL_ARGS[${#SEL_ARGS[@]}]="--$kind"
        SEL_ARGS[${#SEL_ARGS[@]}]="$name"
        r="$(rand_below 5)"
        case "$r" in
            0) filt="float" ;;
            1) filt="semi;colon" ;;
            2) filt='dollar$var' ;;
            3) filt='paren()' ;;
            *) filt="word_$u" ;;
        esac
        gen_crate[$u]="$crate"; gen_kind[$u]="$kind"; gen_name[$u]="$name"
        gen_filter[$u]="$filt"
        SEL_ARGS[${#SEL_ARGS[@]}]="$filt"
        u=$((u + 1))
    done
}

assert_generated_units() {
    local units="$1" u
    [ "$UNIT_COUNT" = "$units" ] || { echo "  unit count $UNIT_COUNT != $units" >&2; return 1; }
    u=0
    while [ "$u" -lt "$units" ]; do
        [ "${UNIT_CRATE[$u]}" = "${gen_crate[$u]}" ] || { echo "  crate $u" >&2; return 1; }
        [ "${UNIT_KIND[$u]}" = "${gen_kind[$u]}" ] || { echo "  kind $u" >&2; return 1; }
        [ "${UNIT_TNAME[$u]}" = "${gen_name[$u]}" ] || { echo "  name $u" >&2; return 1; }
        [ "${UNIT_FILTER[$u]}" = "${gen_filter[$u]}" ] || { echo "  filter $u [${UNIT_FILTER[$u]}] != [${gen_filter[$u]}]" >&2; return 1; }
        [ "${UNIT_KEYS[$u]}" = "${gen_keys[$u]}" ] || { echo "  keys $u [${UNIT_KEYS[$u]}] != [${gen_keys[$u]}]" >&2; return 1; }
        [ "${UNIT_CONSERVATIVE[$u]}" = "${gen_cons[$u]}" ] || { echo "  cons $u" >&2; return 1; }
        u=$((u + 1))
    done
    return 0
}

# ---------------------------------------------------------------------------
# Property A: selection parsing / validation.
# ---------------------------------------------------------------------------
selection_properties() {
    local i units

    # A1: empty selection is rejected before any build.
    SEL_ARGS=()
    check "empty focused selection rejected" nonzero parse_selection

    # A2: unknown crate rejected.
    SEL_ARGS=(-p nope --test t)
    check "unknown crate rejected" nonzero parse_selection

    # A3: unit with no target rejected.
    SEL_ARGS=(-p alpha)
    check "unit without target rejected" nonzero parse_selection

    # A4: two target specs in one unit rejected.
    SEL_ARGS=(-p alpha --test t_alpha --test t_alpha2)
    check "two targets in one unit rejected" nonzero parse_selection

    # A5: unknown option rejected.
    SEL_ARGS=(-p alpha --bogus)
    check "unknown option rejected" nonzero parse_selection

    # A6: filter before any crate rejected.
    SEL_ARGS=(float)
    check "leading filter rejected" nonzero parse_selection

    # A7: bare member crate shorthand opens the first unit.
    SEL_ARGS=(alpha --test t_alpha float)
    if parse_selection >/dev/null 2>&1; then
        check "bare crate shorthand parses" eq alpha "${UNIT_CRATE[0]}"
        check "bare crate shorthand keeps filter" eq float "${UNIT_FILTER[0]}"
    else
        check "bare crate shorthand parses" false
    fi

    # A8: isolation marker semantics.
    SEL_ARGS=(-p alpha --isolated --test t_alpha f)
    parse_selection >/dev/null 2>&1
    check "isolated -> non-conservative" eq 0 "${UNIT_CONSERVATIVE[0]}"
    check "isolated -> no exclusive key" eq "" "${UNIT_KEYS[0]}"
    SEL_ARGS=(-p alpha --group g --test t_alpha f)
    parse_selection >/dev/null 2>&1
    check "group -> non-conservative" eq 0 "${UNIT_CONSERVATIVE[0]}"
    check "group recorded" eq ";group:g" "${UNIT_KEYS[0]}"
    SEL_ARGS=(-p alpha --test t_alpha f)
    parse_selection >/dev/null 2>&1
    check "no marker -> conservative" eq 1 "${UNIT_CONSERVATIVE[0]}"

    # A9: generated multi-unit selections round-trip exactly.
    local budget="${PROP_SELECTION_CASES:-120}"
    i=0
    while [ "$i" -lt "$budget" ]; do
        units=$(( $(rand_below 4) + 1 ))
        generate_selection "$units"
        check "generated selection parses (case $i units=$units)" parse_selection
        check "generated units round-trip (case $i)" assert_generated_units "$units"
        i=$((i + 1))
    done

    # A10: shell metacharacters are inert (never evaluated).
    local sentinel="$OUT/injection-sentinel"
    rm -f "$sentinel"
    SEL_ARGS=(-p alpha --test t_alpha "\$(touch $sentinel)")
    parse_selection >/dev/null 2>&1
    check "injection token preserved literally" eq "\$(touch $sentinel)" "${UNIT_FILTER[0]}"
    check "injection produced no side effect" test ! -e "$sentinel"
}

# ---------------------------------------------------------------------------
# Property B: bounded concurrency and exclusion.
# ---------------------------------------------------------------------------
SCHED_DIR="$OUT/schedule"
mkdir -p "$SCHED_DIR"
ISO_SPECS=()

reset_sched_jobs() {
    JOB_COUNT=${#ISO_SPECS[@]}
    JOB_LABEL=(); JOB_CWD=(); JOB_EXE=(); JOB_ARGV=(); JOB_KEYS=()
    JOB_CONSERVATIVE=(); JOB_LOG=(); JOB_RC=(); JOB_START_MS=(); JOB_END_MS=()
    JOB_OVERLAP=(); JOB_DISCOVERED=(); JOB_PASSED=(); JOB_FAILED=(); JOB_IGNORED=()
    JOB_OK=(); JOB_PID=(); JOB_UNRUNNABLE=()
    local j=0
    while [ "$j" -lt "$JOB_COUNT" ]; do
        local spec="${ISO_SPECS[$j]}"
        JOB_LABEL[$j]="job$j"
        JOB_CWD[$j]="$repo_root"
        JOB_EXE[$j]="sleep"
        JOB_ARGV[$j]="0.2"
        JOB_LOG[$j]="$SCHED_DIR/job$j.log"
        JOB_DISCOVERED[$j]=1
        case "$spec" in
            I) JOB_KEYS[$j]=""; JOB_CONSERVATIVE[$j]=0 ;;
            C) JOB_KEYS[$j]=""; JOB_CONSERVATIVE[$j]=1 ;;
            G:*) JOB_KEYS[$j]=";group:${spec#G:}"; JOB_CONSERVATIVE[$j]=0 ;;
        esac
        j=$((j + 1))
    done
}

overlap_has() { # $1 newline list, $2 index
    local line
    while IFS= read -r line; do
        [ "$line" = "$2" ] && return 0
    done <<< "$1"
    return 1
}

# Assert no conflicting pair overlapped. Uses the exact launch-time overlap map,
# so it is independent of wall-clock resolution.
assert_conflict_free() {
    local i j
    i=0
    while [ "$i" -lt "$JOB_COUNT" ]; do
        j=0
        while [ "$j" -lt "$JOB_COUNT" ]; do
            if [ "$i" != "$j" ] && exclusion_conflict "${JOB_KEYS[$i]}" "${JOB_CONSERVATIVE[$i]}" "${JOB_KEYS[$j]}" "${JOB_CONSERVATIVE[$j]}"; then
                if overlap_has "${JOB_OVERLAP[$j]}" "$i" || overlap_has "${JOB_OVERLAP[$i]}" "$j"; then
                    echo "  conflicting jobs $i and $j overlapped" >&2
                    return 1
                fi
            fi
            j=$((j + 1))
        done
        i=$((i + 1))
    done
    return 0
}

all_jobs_ran() {
    local j=0
    while [ "$j" -lt "$JOB_COUNT" ]; do
        [ -f "$SCHED_DIR/jobs/$j.status" ] || { echo "  job $j did not run" >&2; return 1; }
        j=$((j + 1))
    done
    return 0
}

run_schedule() { # $1 = effective jobs
    EFF_JOBS="$1"
    JOB_STATUS_DIR="$SCHED_DIR/jobs"
    rm -rf "$JOB_STATUS_DIR"; mkdir -p "$JOB_STATUS_DIR"
    reset_sched_jobs
    run_pool
}

scheduling_properties() {
    # B1: two isolated jobs overlap under jobs=2.
    ISO_SPECS=(I I)
    run_schedule 2
    check "two isolated jobs reach peak 2" eq 2 "$PEAK_CONCURRENCY"
    check "two isolated jobs both run" all_jobs_ran
    check "two isolated jobs conflict-free (trivially)" assert_conflict_free

    # B2: two conservative jobs serialize.
    ISO_SPECS=(C C)
    run_schedule 2
    check "two conservative jobs peak 1" eq 1 "$PEAK_CONCURRENCY"
    check "two conservative jobs conflict-free" assert_conflict_free

    # B3: same exclusion group serializes.
    ISO_SPECS=(G:shared G:shared)
    run_schedule 2
    check "same group peak 1" eq 1 "$PEAK_CONCURRENCY"
    check "same group conflict-free" assert_conflict_free

    # B4: distinct groups may overlap.
    ISO_SPECS=(G:a G:b)
    run_schedule 2
    check "distinct groups peak 2" eq 2 "$PEAK_CONCURRENCY"
    check "distinct groups conflict-free" assert_conflict_free

    # B5: jobs=1 always serializes regardless of isolation.
    ISO_SPECS=(I I I)
    run_schedule 1
    check "jobs=1 peak 1" eq 1 "$PEAK_CONCURRENCY"
    check "jobs=1 all run" all_jobs_ran

    # Generated schedules: peak bound and conflict-free overlap always hold.
    local budget="${PROP_SCHEDULE_CASES:-20}" i n j spec
    i=0
    while [ "$i" -lt "$budget" ]; do
        n=$(( $(rand_below 4) + 1 ))
        ISO_SPECS=()
        j=0
        while [ "$j" -lt "$n" ]; do
            case "$(rand_below 4)" in
                0) ISO_SPECS[${#ISO_SPECS[@]}]="I" ;;
                1) ISO_SPECS[${#ISO_SPECS[@]}]="C" ;;
                2) ISO_SPECS[${#ISO_SPECS[@]}]="G:g0" ;;
                *) ISO_SPECS[${#ISO_SPECS[@]}]="G:g1" ;;
            esac
            j=$((j + 1))
        done
        run_schedule 2
        check "generated schedule $i peak<=2 (peak=$PEAK_CONCURRENCY)" test "$PEAK_CONCURRENCY" -le 2
        check "generated schedule $i all run" all_jobs_ran
        check "generated schedule $i conflict-free" assert_conflict_free
        i=$((i + 1))
    done
}

# ---------------------------------------------------------------------------
# Property C: outcome aggregation.
# ---------------------------------------------------------------------------
AGG_RC=()
AGG_P=()
AGG_F=()
AGG_I=()

setup_agg() {
    JOB_COUNT=${#AGG_RC[@]}
    JOB_PASSED=(); JOB_FAILED=(); JOB_IGNORED=(); JOB_RC=(); JOB_OK=(); JOB_UNRUNNABLE=()
    JOB_START_MS=(); JOB_END_MS=(); JOB_OVERLAP=()
    local j=0
    while [ "$j" -lt "$JOB_COUNT" ]; do
        JOB_RC[$j]="${AGG_RC[$j]}"
        JOB_PASSED[$j]="${AGG_P[$j]}"
        JOB_FAILED[$j]="${AGG_F[$j]}"
        JOB_IGNORED[$j]="${AGG_I[$j]}"
        j=$((j + 1))
    done
    INTERRUPTED=0
}

expected_status() {
    local j=0
    while [ "$j" -lt "${#AGG_RC[@]}" ]; do
        [ "${AGG_RC[$j]}" != "0" ] && { echo failed; return; }
        [ "${AGG_P[$j]}" = "0" ] && { echo failed; return; }
        j=$((j + 1))
    done
    echo passed
}

assert_agg_invariants() {
    local want got
    want="$(expected_status)"
    got="$AGG_STATUS"
    [ "$want" = "$got" ] || { echo "  status [$got] != [$want]" >&2; return 1; }
    local sp=0 sf=0 si=0 j=0
    while [ "$j" -lt "${#AGG_RC[@]}" ]; do
        sp=$((sp + AGG_P[$j])); sf=$((sf + AGG_F[$j])); si=$((si + AGG_I[$j]))
        j=$((j + 1))
    done
    [ "$AGG_PASSED" = "$sp" ] || { echo "  passed total ${AGG_PASSED} != $sp" >&2; return 1; }
    [ "$AGG_FAILED" = "$sf" ] || { echo "  failed total" >&2; return 1; }
    [ "$AGG_IGNORED" = "$si" ] || { echo "  ignored total" >&2; return 1; }
    return 0
}

aggregation_properties() {
    # C1: all passing.
    AGG_RC=(0 0); AGG_P=(3 2); AGG_F=(0 0); AGG_I=(0 0)
    setup_agg; aggregate_results
    check "all passing -> passed" eq passed "$AGG_STATUS"
    check "all passing totals" assert_agg_invariants

    # C2: one non-zero exit fails the run.
    AGG_RC=(0 1); AGG_P=(3 0); AGG_F=(0 1); AGG_I=(0 0)
    setup_agg; aggregate_results
    check "nonzero exit -> failed" eq failed "$AGG_STATUS"
    check "nonzero exit totals" assert_agg_invariants

    # C3: ignored-only (zero passed) fails the run.
    AGG_RC=(0 0); AGG_P=(2 0); AGG_F=(0 0); AGG_I=(0 5)
    setup_agg; aggregate_results
    check "ignored-only -> failed" eq failed "$AGG_STATUS"
    check "ignored-only totals" assert_agg_invariants

    # C4: failure propagates past passing selections.
    AGG_RC=(0 0 0); AGG_P=(1 1 0); AGG_F=(0 0 0); AGG_I=(0 0 4)
    setup_agg; aggregate_results
    check "failure propagates" eq failed "$AGG_STATUS"
    check "failure propagates totals" assert_agg_invariants

    # Generated result sequences.
    local budget="${PROP_AGG_CASES:-200}" i n j
    i=0
    while [ "$i" -lt "$budget" ]; do
        n=$(( $(rand_below 4) + 1 ))
        AGG_RC=(); AGG_P=(); AGG_F=(); AGG_I=()
        j=0
        while [ "$j" -lt "$n" ]; do
            AGG_RC[$j]="$(rand_below 3)"        # 0,1,2 -> two failure kinds
            AGG_P[$j]="$(rand_below 4)"
            AGG_F[$j]="$(rand_below 2)"
            AGG_I[$j]="$(rand_below 3)"
            j=$((j + 1))
        done
        setup_agg; aggregate_results
        check "generated aggregate $i" assert_agg_invariants
        i=$((i + 1))
    done
}

# ---------------------------------------------------------------------------
# Property D: evidence logging.
# ---------------------------------------------------------------------------
# run_logged must create the parent directory of each evidence base (the
# prerequisite builds write under build/ before compile_crate creates it) and
# must fail when it cannot record the command or raw log, instead of letting the
# command's own status mask the evidence-write failure.
evidence_complete() { # $1 = evidence base prefix
    [ -s "$1.command" ] && [ -f "$1.log" ] && [ -s "$1.status" ] && [ -s "$1.duration" ]
}

evidence_properties() {
    local base_dir="$OUT/evidence"
    rm -rf "$base_dir"; mkdir -p "$base_dir"
    local saved_run_dir="$RUN_DIR"
    RUN_DIR="$base_dir/run"

    # D1: a nested base creates its parent directory and records all four files.
    if run_logged "build/prereq-probe" true >/dev/null 2>&1; then
        check "evidence nested base complete" evidence_complete "$RUN_DIR/build/prereq-probe"
        check "evidence command names argv" bash -c "grep -q true '$RUN_DIR/build/prereq-probe.command'"
    else
        check "evidence nested base complete" false
    fi

    # D2: a non-zero command status propagates.
    check "evidence nonzero status propagates" nonzero run_logged "build/fail-probe" false

    # D3: unwritable evidence fails the run (a parent path component is a file).
    : > "$RUN_DIR/blocker"
    check "evidence unwritable base fails" nonzero run_logged "blocker/app" true

    # Generated: random nested depths all yield complete evidence.
    local i depth path segment
    i=0
    while [ "$i" -lt "${PROP_EVIDENCE_CASES:-20}" ]; do
        depth=$(( $(rand_below 4) + 1 ))
        path=""; segment=0
        while [ "$segment" -lt "$depth" ]; do
            path="$path/d$segment"
            segment=$((segment + 1))
        done
        path="${path#/}"
        run_logged "gen$i/$path/base" true >/dev/null 2>&1
        check "evidence generated nested case $i (depth=$depth)" \
            evidence_complete "$RUN_DIR/gen$i/$path/base"
        i=$((i + 1))
    done

    RUN_DIR="$saved_run_dir"
}

# ---------------------------------------------------------------------------
main() {
    report "runner property tests seed=$SEED selection_cases=${PROP_SELECTION_CASES:-120} schedule_cases=${PROP_SCHEDULE_CASES:-20} agg_cases=${PROP_AGG_CASES:-200} evidence_cases=${PROP_EVIDENCE_CASES:-20}"
    selection_properties
    scheduling_properties
    aggregation_properties
    evidence_properties
    report "PASS=$PASS FAIL=$FAIL"
    {
        printf 'seed=%s\nselection_cases=%s\nschedule_cases=%s\naggregation_cases=%s\nevidence_cases=%s\npass=%s\nfail=%s\n' \
            "$SEED" "${PROP_SELECTION_CASES:-120}" "${PROP_SCHEDULE_CASES:-20}" \
            "${PROP_AGG_CASES:-200}" "${PROP_EVIDENCE_CASES:-20}" "$PASS" "$FAIL"
        [ -n "$FAILED" ] && printf 'failed:%s\n' "$FAILED"
    } > "$OUT/properties-receipt.txt"
    echo "[properties] pass=$PASS fail=$FAIL (seed=$SEED) evidence=$OUT"
    [ "$FAIL" -eq 0 ] || return 1
    return 0
}

main
