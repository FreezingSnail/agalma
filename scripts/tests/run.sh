#!/usr/bin/env bash
# Entry point for the focused runner's own verification suite.
#
#   bash scripts/tests/run.sh
#
# Runs the bounded property tests (selection isolation, concurrency/exclusion
# bounds, outcome aggregation) and the end-to-end acceptance cases, all under
# one run id at the persistent artifact root:
#
#   $RUN_ROOT/$BEAD/$RUN_ID_BASE/
#     properties/   property-test logs and receipt
#     integration/  per-case runner logs and receipt
#     SUMMARY.md    combined outcome
#
# Environment: BEAD (default agalma-i15), RUN_ROOT, RUN_ID_BASE.

set -o pipefail

repo_root="$(cd -- "$(dirname -- "${BASH_SOURCE[0]}")/../.." && pwd)"
cd "$repo_root" || exit 1

# shellcheck source=../test.sh
source "$repo_root/scripts/test.sh"

BEAD="${BEAD:-agalma-i15}"
RUN_ROOT="${RUN_ROOT:-$(default_run_root)}"
RUN_ID_BASE="${RUN_ID_BASE:-$(date -u +%Y%m%dT%H%M%SZ)-$$}"
export BEAD RUN_ROOT RUN_ID_BASE
export RUN_ID="$RUN_ID_BASE"

root="$RUN_ROOT/$BEAD/$RUN_ID_BASE"
mkdir -p "$root"

echo "[suite] running property tests"
bash "$repo_root/scripts/tests/runner-properties.sh"
props_rc=$?

echo "[suite] running integration cases"
bash "$repo_root/scripts/tests/runner-focused.sh"
integ_rc=$?

status="passed"
{ [ "$props_rc" -ne 0 ] || [ "$integ_rc" -ne 0 ]; } && status="failed"

{
    echo "# focused-runner verification suite"
    echo
    echo "- bead: $BEAD"
    echo "- run id: $RUN_ID_BASE"
    echo "- root: $root"
    echo "- git: $(git rev-parse HEAD 2>/dev/null || echo unknown) ($(git rev-parse --abbrev-ref HEAD 2>/dev/null || echo unknown))"
    echo "- status: $status"
    echo "- property tests exit: $props_rc"
    echo "- integration tests exit: $integ_rc"
    echo
    echo "## evidence"
    echo "- properties: $root/properties/"
    echo "- integration: $root/integration/"
} > "$root/SUMMARY.md"

cat "$root/SUMMARY.md"
echo "[suite] status=$status"
{ [ "$props_rc" -eq 0 ] && [ "$integ_rc" -eq 0 ]; } || exit 1
exit 0
