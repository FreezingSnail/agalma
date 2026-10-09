#!/bin/sh
# S0b probe — acceptance: external provider auth proxy + narrow conductor bridge.
# Proves the worker reaches a parent-owned loopback auth proxy and a parent-owned
# unix nerve socket, while being unable to read the credential file itself or
# reach any off-path socket/state.
# Usage: p07-authproxy-bridge.sh <rundir> [repo]
set -u
RUN="$(cd "$1" && pwd -P)"
S0B="$(cd "$(dirname "$0")/.." && pwd -P)"
RUN_SH="$S0B/probes/run.sh"
NODE="$(command -v node)"
A="$RUN/attempt"; PROT="$RUN/protected"; SOCK="$RUN/sockets"
PROXY_PORT=39079

sandbox() { "$RUN_SH" "$A" "$PROT" "$SOCK" "$@" 2>&1; }
chk() { if [ "$2" = "$3" ]; then echo "RESULT $1 PASS exit=$3 :: $4"; else echo "RESULT $1 FAIL want=$2 got=$3 :: $4"; fi; }

PROXY_PID=""; BRIDGE_PID=""
cleanup(){ [ -n "$PROXY_PID" ] && kill "$PROXY_PID" 2>/dev/null; [ -n "$BRIDGE_PID" ] && kill "$BRIDGE_PID" 2>/dev/null; }
trap cleanup EXIT INT TERM

echo "== auth proxy + nerve bridge =="
# credential lives outside the worker
echo '{"token":"DUMMY-SECRET"}' > "$RUN/state/auth.json"

# parent-owned auth proxy (holds the credential; worker never sees it)
"$NODE" "$S0B/probes/net-http.js" "$PROXY_PORT" > "$RUN/proxy.log" 2>&1 &
PROXY_PID=$!
"$NODE" "$S0B/probes/net-uds.js" "$SOCK/nerve.sock" > "$RUN/bridge.log" 2>&1 &
BRIDGE_PID=$!
sleep 1

out=$(sandbox /usr/bin/curl -s -m 3 "http://127.0.0.1:$PROXY_PORT/"); rc=$?
echo "WORKER_VIA_PROXY[$rc]: $out"
chk authproxy.worker-reaches-proxy 0 "$rc" "$out"

out=$(sandbox /bin/cat "$RUN/state/auth.json"); rc=$?
echo "WORKER_READ_CREDENTIAL[$rc]: $out"
chk authproxy.worker-cannot-read-credential 1 "$rc" "$out"

# narrow bridge: worker may connect to the nerve socket
out=$(printf '{"op":"report"}\n' | sandbox /usr/bin/nc -U -w 2 "$SOCK/nerve.sock"); rc=$?
case "$out" in *NERVE-OK*) bres=0;; *) bres=1;; esac
echo "WORKER_VIA_BRIDGE: $out"
chk bridge.worker-reaches-nerve 0 "$bres" "$out"

# ...but not to conductor state or any off-path socket
out=$(sandbox /bin/ls "$RUN/state"); rc=$?
echo "WORKER_LIST_STATE[$rc]: $out"
chk bridge.worker-cannot-list-state 1 "$rc" "$out"

cleanup; PROXY_PID=""; BRIDGE_PID=""
