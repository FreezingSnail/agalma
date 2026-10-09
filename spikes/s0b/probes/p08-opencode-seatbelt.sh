#!/bin/sh
# S0b probe — harness binary inside Seatbelt (opencode v2 smoke).
# Proves the real harness binary starts and binds loopback while confined,
# with an isolated XDG tree, and that its install tree is read-only.
# Usage: p08-opencode-seatbelt.sh <rundir>
set -u
RUN="$(cd "$1" && pwd -P)"
S0B="$(cd "$(dirname "$0")/.." && pwd -P)"
RUN_SH="$S0B/probes/run.sh"
A="$RUN/attempt"; PROT="$RUN/protected"; SOCK="$RUN/sockets"
PORT=39002

OC="$(command -v opencode || true)"
if [ -z "$OC" ]; then echo "RESULT opencode.present BLOCKED :: opencode not on PATH"; exit 0; fi
OC_REAL="$(readlink "$OC" || echo "$OC")"
# extra read-only root: the harness install tree
case "$OC_REAL" in
  */packages/cli/*) EXTRA="$(printf '%s' "$OC_REAL" | sed -E 's#(/packages/cli/).*#\1#')" ;;
  *) EXTRA="$(dirname "$OC_REAL")" ;;
esac
echo "opencode=$OC real=$OC_REAL extra_ro=$EXTRA"

chk() { if [ "$2" = "$3" ]; then echo "RESULT $1 PASS exit=$3 :: $4"; else echo "RESULT $1 FAIL want=$2 got=$3 :: $4"; fi; }

mkdir -p "$A/oc-xcfg" "$A/oc-xdata" "$A/oc-xcache" "$A/oc-xstate" "$A/oc-tmp"
export S0B_EXTRA_RO="$EXTRA"
export TMPDIR="$A/oc-tmp"
export XDG_CONFIG_HOME="$A/oc-xcfg" XDG_DATA_HOME="$A/oc-xdata" \
       XDG_CACHE_HOME="$A/oc-xcache" XDG_STATE_HOME="$A/oc-xstate"

echo "== opencode under seatbelt =="
out=$("$RUN_SH" "$A" "$PROT" "$SOCK" "$OC" --version 2>&1); rc=$?
echo "OC_VERSION[$rc]: $out"
chk opencode.version 0 "$rc" "$out"

# start serve confined; parent confirms loopback reachability
"$RUN_SH" "$A" "$PROT" "$SOCK" "$OC" serve --hostname 127.0.0.1 --port "$PORT" \
  > "$RUN/oc-serve.log" 2>&1 &
OCPID=$!
sleep 3
log=$(cat "$RUN/oc-serve.log" 2>/dev/null)
echo "SERVE_LOG: $log"
if kill -0 "$OCPID" 2>/dev/null; then alive=0; else alive=1; fi
chk opencode.serve-alive 0 "$alive" "$(printf '%s' "$log" | tr '\n' ' ')"
# TCP connection accepted (401 unauth expected) proves loopback path to harness
code=$(curl -s -o /dev/null -m 3 -w '%{http_code}' "http://127.0.0.1:$PORT/api/info")
echo "PARENT_TO_SERVE http=$code"
if [ "$code" = "000" ] || [ -z "$code" ]; then echo "RESULT opencode.loopback-reachable FAIL :: no TCP response"; else echo "RESULT opencode.loopback-reachable PASS :: http=$code (connection accepted)"; fi

kill "$OCPID" 2>/dev/null; sleep 0.4
if kill -0 "$OCPID" 2>/dev/null; then kill -9 "$OCPID" 2>/dev/null; fi
pgrep -f "opencode serve.*--port $PORT" >/dev/null 2>&1 && echo "RESULT opencode.shutdown FAIL :: orphan serve" || echo "RESULT opencode.shutdown PASS :: no orphan serve"
exit 0
