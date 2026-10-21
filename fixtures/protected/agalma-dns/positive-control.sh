#!/usr/bin/env bash
# Positive-control test for the protected agalma-dns judge.
#
# The judge must ACCEPT a conforming runner that delivers job logs/status under
# an absolute RUN_DIR despite changing the job cwd, for relative/default/
# absolute roots. The control runner is independently specified in
# `control/scripts/test.sh`; it is not a fix to the product `scripts/test.sh`.
#
# It also proves repeat-invocation preservation: two judge invocations with the
# same explicit scratch base must create distinct invocation roots and must not
# delete the first run's evidence. A final pair runs the exact emitted manifest
# command (no scratch/run overrides) with the primary persistent evidence base.
#
#   bash fixtures/protected/agalma-dns/positive-control.sh
#
# Evidence stays under <primary>/target/test-runs/agalma-dns/positive-control/.

set -euo pipefail

here="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd -P)"
judge="$here/judge.sh"
manifest="${AGALMA_DNS_MANIFEST:-}"
primary="$(git worktree list --porcelain | sed -n 's/^worktree //p' | head -n 1)"
[ -n "$primary" ] || { echo "positive-control: no primary worktree" >&2; exit 1; }
evidence="$primary/target/test-runs/agalma-dns"
invocation="$(date -u +%Y%m%dT%H%M%SZ)-$$"
out="$evidence/positive-control/$invocation"
control="$out/control"
mkdir -p "$control/scripts" "$out/protected/agalma-dns" "$out/exact-protected/agalma-dns"

cp "$here/control/scripts/test.sh" "$control/scripts/test.sh"
chmod 755 "$control/scripts/test.sh"
cp "$judge" "$out/protected/agalma-dns/judge.sh"
cp "$judge" "$out/exact-protected/agalma-dns/judge.sh"
chmod 755 "$out/protected/agalma-dns/judge.sh" "$out/exact-protected/agalma-dns/judge.sh"

fail=0
run_explicit() {
    local pass="$1"
    local log="$out/explicit-$pass.log"
    if AGALMA_CANDIDATE_DIR="$control" \
        AGALMA_PROTECTED_DIR="$out/protected" \
        AGALMA_JUDGE_SCRATCH="$out/scratch" \
        AGALMA_JUDGE_EVIDENCE_BASE="$out/runs" \
        AGALMA_JUDGE_BEAD=agalma-dns-control \
        AGALMA_JUDGE_TIMEOUT=8 \
        bash "$judge" >"$log" 2>&1; then
        printf 'explicit pass %s: exit=0\n' "$pass"
    else
        local rc=$?
        printf 'explicit pass %s: exit=%s\n' "$pass" "$rc"
        fail=1
    fi
    grep -q '^PASS relative-root:' "$log" || { echo "missing PASS relative-root" >&2; fail=1; }
    grep -q '^PASS default-root:' "$log" || { echo "missing PASS default-root" >&2; fail=1; }
    grep -q '^PASS absolute-root:' "$log" || { echo "missing PASS absolute-root" >&2; fail=1; }
}

echo "positive-control: explicit scratch preservation"
first_before="$((ls -d "$out/scratch"/invocation-* 2>/dev/null || true) | wc -l | tr -d ' ')"
run_explicit 1
first_inv="$(ls -d "$out/scratch"/invocation-* | head -n 1)"
run_explicit 2
first_after="$(ls -d "$out/scratch"/invocation-* | wc -l | tr -d ' ')"
[ "$first_before" = "0" ] || { echo "unexpected pre-existing scratch" >&2; fail=1; }
[ "$first_after" -ge 2 ] || { echo "repeat invocation did not append" >&2; fail=1; }
[ -d "$first_inv" ] || { echo "first invocation evidence was removed" >&2; fail=1; }
[ "$(find "$first_inv" -name '00-judge.log' | wc -l | tr -d ' ')" -ge 2 ] || {
    echo "first invocation relative/absolute job evidence was not preserved" >&2
    fail=1
}
first_default="$(sed -n 's#^PASS default-root: log=\(.*\)/jobs/00-judge.log status=.*#\1#p' "$out/explicit-1.log")"
[ -n "$first_default" ] || { echo "first default-root evidence path missing" >&2; fail=1; }
[ -f "$first_default/jobs/00-judge.log" ] || {
    echo "first invocation default-root evidence was not preserved" >&2
    fail=1
}

echo "positive-control: exact emitted command preservation"
# Deliberately hostile evidence base: spaces and an embedded single quote.
exact_evidence="$out/evidence base 'quoted'"
exact_manifest="$out/exact-manifest.json"
exact_protected="$out/protected root with spaces 'and quote'"
mkdir -p "$exact_evidence" "$exact_protected"

bash "$here/render-manifest.sh" \
    --repository-root "$primary" \
    --base-sha "$(git -C "$primary" rev-parse main)" \
    --evidence-base "$exact_evidence" >"$exact_manifest"

# Materialize the manifest baseline exactly as the protected runner does.
file_count="$(jq '.entries[0].baseline.files | length' "$exact_manifest")"
i=0
while [ "$i" -lt "$file_count" ]; do
    rel="$(jq -r ".entries[0].baseline.files[$i].path" "$exact_manifest")"
    mode="$(jq -r ".entries[0].baseline.files[$i].mode" "$exact_manifest")"
    mkdir -p "$exact_protected/$(dirname "$rel")"
    jq -j --argjson i "$i" '.entries[0].baseline.files[$i].text' "$exact_manifest" >"$exact_protected/$rel"
    case "$mode" in
        100755) chmod 755 "$exact_protected/$rel" ;;
        100644) chmod 644 "$exact_protected/$rel" ;;
        *) echo "positive-control: unsupported materialized mode $mode" >&2; fail=1 ;;
    esac
    i=$((i + 1))
done

exact_command="$(jq -r '.entries[0].baseline.commands[0]' "$exact_manifest")"
printf 'exact command: %s\n' "$exact_command" >"$out/exact-commands.txt"
printf 'exact evidence: %s\n' "$exact_evidence" >>"$out/exact-commands.txt"
printf 'exact protected: %s\n' "$exact_protected" >>"$out/exact-commands.txt"

assert_pass_evidence() {
    local log="$1"
    local line rest log_path status_path
    while IFS= read -r line; do
        case "$line" in
            PASS\ *) ;;
            *) continue ;;
        esac
        case "$line" in
            *"log="*" status="*) ;;
            *) echo "malformed PASS line: $line" >&2; fail=1; continue ;;
        esac
        rest="${line#*log=}"
        log_path="${rest%% status=*}"
        status_path="${rest##* status=}"
        [ -f "$log_path" ] || { echo "missing exact log: $log_path" >&2; fail=1; }
        [ -f "$status_path" ] || { echo "missing exact status: $status_path" >&2; fail=1; }
    done <"$log"
}

run_exact() {
    local pass="$1"
    local log="$out/exact-command-$pass.log"
    local rc=0
    # Execute the literal emitted command through a production-equivalent
    # shell. Only the parent roots are supplied; all judge run/scratch/evidence
    # overrides are cleared so the command text alone controls the launch.
    env -u AGALMA_JUDGE_EVIDENCE_BASE \
        -u AGALMA_JUDGE_SCRATCH \
        -u AGALMA_JUDGE_BEAD \
        -u AGALMA_JUDGE_TIMEOUT \
        AGALMA_CANDIDATE_DIR="$control" \
        AGALMA_PROTECTED_DIR="$exact_protected" \
        bash -c "$exact_command" >"$log" 2>&1 || rc=$?
    printf 'exact pass %s: exit=%s\n' "$pass" "$rc"
    [ "$rc" -eq 0 ] || fail=1
    grep -q '^PASS relative-root:' "$log" || { echo "missing exact PASS relative-root" >&2; fail=1; }
    grep -q '^PASS default-root:' "$log" || { echo "missing exact PASS default-root" >&2; fail=1; }
    grep -q '^PASS absolute-root:' "$log" || { echo "missing exact PASS absolute-root" >&2; fail=1; }
    assert_pass_evidence "$log"
}

run_exact 1
exact_first="$(ls -d "$exact_evidence/scratch"/invocation-* | head -n 1)"
first_default="$(sed -n 's#^PASS default-root: log=\(.*\)/jobs/00-judge.log status=.*#\1#p' "$out/exact-command-1.log")"
run_exact 2
exact_after="$(ls -d "$exact_evidence/scratch"/invocation-* | wc -l | tr -d ' ')"
[ "$exact_after" -ge 2 ] || { echo "exact runs did not append" >&2; fail=1; }
[ -d "$exact_first" ] || { echo "exact first invocation evidence removed" >&2; fail=1; }
[ "$(find "$exact_first" -name '00-judge.log' | wc -l | tr -d ' ')" -ge 2 ] || {
    echo "exact first invocation relative/absolute evidence was not preserved" >&2
    fail=1
}
[ -n "$first_default" ] || { echo "exact first default-root path missing" >&2; fail=1; }
[ -f "$first_default/jobs/00-judge.log" ] || {
    echo "exact first invocation default-root evidence was not preserved" >&2
    fail=1
}

echo "positive-control: evidence=$out"
[ "$fail" -eq 0 ]
