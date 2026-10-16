#!/usr/bin/env bash
# FEZ acceptance compatibility entry: real local processes on the default
# Connect wire, no paid inference.
#
# The implementation now lives in the unified runner (scripts/test.sh peers):
# workspace-bin build, per-server executable preflight, the five required
# process smokes, and the retained conductor suite. This script forwards to it
# so there is exactly one implementation and callers keep the same acceptance
# behavior. `--print` and any other arguments pass through unchanged.
set -euo pipefail

workspace_dir="$(cd -- "$(dirname -- "${BASH_SOURCE[0]}")/.." && pwd)"
cd "$workspace_dir"

exec bash scripts/test.sh peers "$@"
