#!/bin/sh
# S0b probe helper: run a command under the confine profile.
# Usage: run.sh <attempt-dir> <protected-dir> <sockdir> <cmd...>
# Resolves absolute paths, substitutes profile params.
set -u
ROOT="$(cd "$(dirname "$0")/../../.." && pwd)"     # repo root
PROFILE="$ROOT/spikes/s0b/profiles/worker.sb"
ATTEMPT="$(cd "$1" && pwd -P)"; shift
PROTECTED="$(cd "$1" && pwd -P)"; shift
SOCKDIR="$(cd "$1" && pwd -P)"; shift
# Optional extra read-only root for the harness install tree.
EXTRA_RO="${S0B_EXTRA_RO:-$ATTEMPT}"
cd "$ATTEMPT" || exit 99
exec /usr/bin/sandbox-exec \
    -f "$PROFILE" \
    -D "ATTEMPT=$ATTEMPT" \
    -D "PROTECTED=$PROTECTED" \
    -D "SOCKDIR=$SOCKDIR" \
    -D "EXTRA_RO=$EXTRA_RO" \
    "$@"
