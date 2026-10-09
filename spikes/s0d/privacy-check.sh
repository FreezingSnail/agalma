#!/usr/bin/env bash
# S0d seam-freeze privacy check.
#
# The scenario driver and the frozen contract must contain no vendor leakage:
# no OpenCode names, endpoints, transports, auth, native IDs, or prompt text.
# Only the adapter module may mention them.
#
# Usage:
#   privacy-check.sh                 # check the default targets
#   privacy-check.sh <file> [...]    # check specific files (used by the
#                                    # negative self-test)
#
# Exit 0 when clean, 1 when any target leaks a vendor detail.

set -uo pipefail

here="$(cd "$(dirname "$0")" && pwd)"

if [ "$#" -gt 0 ]; then
  targets=("$@")
else
  targets=("$here/src/scenario.rs" "$here/src/contract.rs")
fi

# Vendor-leakage vocabulary. Canonical Agalma IDs such as `session:1:1` and
# capability strings such as `synthetic_feedback` must NOT match.
pattern='opencode|OpenCode|OPENCODE|/api/|127\.0\.0\.1|localhost|TcpStream|text/event-stream|EventSource|Authorization: Basic|Basic auth|sessionID|providerID|mimo-v2|--hostname|--port|\bcurl\b|\bSSE\b|\binterrupt\b|XDG_|serve --|ses_'

leaks=0
for target in "${targets[@]}"; do
  if [ ! -f "$target" ]; then
    echo "privacy-check: missing target $target"
    leaks=1
    continue
  fi
  if grep -nE "$pattern" "$target" >/dev/null 2>&1; then
    echo "privacy-check: VENDOR LEAKAGE in $target"
    grep -nE "$pattern" "$target"
    leaks=1
  else
    echo "privacy-check: clean $target"
  fi
done

if [ "$leaks" -eq 0 ]; then
  echo "privacy-check: PASS"
  exit 0
fi
echo "privacy-check: FAIL"
exit 1
