#!/bin/sh
# S0b probe — rows 7/8/9: network confinement.
#   row7 loopback HTTP to parent-owned server allowed; worker may bind loopback.
#   row8 parent-owned unix socket (nerve bridge) allowed; off-path socket denied.
#   row9 arbitrary external network denied.
# Usage: p04-network.sh <rundir> [repo]
set -u
RUN="$(cd "$1" && pwd -P)"
S0B="$(cd "$(dirname "$0")/.." && pwd -P)"
RUN_SH="$S0B/probes/run.sh"
NODE="$(command -v node)"
A="$RUN/attempt"; PROT="$RUN/protected"; SOCK="$RUN/sockets"
HTTP_PORT=39077; BIND_PORT=39078

sandbox() { "$RUN_SH" "$A" "$PROT" "$SOCK" "$@" 2>&1; }
chk() { if [ "$2" = "$3" ]; then echo "RESULT $1 PASS exit=$3 :: $4"; else echo "RESULT $1 FAIL want=$2 got=$3 :: $4"; fi; }

HTTP_PID=""; UDS_PID=""; FORBID_PID=""
cleanup(){ [ -n "$HTTP_PID" ] && kill "$HTTP_PID" 2>/dev/null; [ -n "$UDS_PID" ] && kill "$UDS_PID" 2>/dev/null; [ -n "$FORBID_PID" ] && kill "$FORBID_PID" 2>/dev/null; }
trap cleanup EXIT INT TERM

echo "== row7 loopback HTTP =="
"$NODE" "$S0B/probes/net-http.js" "$HTTP_PORT" > "$RUN/net-http.log" 2>&1 &
HTTP_PID=$!
sleep 1
# baseline: parent (unconfined) reaches the server
presp=$(curl -s -m 2 "http://127.0.0.1:$HTTP_PORT/"); prc=$?
echo "PARENT_CURL[$prc]: $presp"
chk row7.parent-baseline 0 "$prc" "$presp"
# confined client reaches parent-owned loopback server
out=$(sandbox /usr/bin/curl -s -m 2 "http://127.0.0.1:$HTTP_PORT/"); rc=$?
echo "SANDBOX_CURL[$rc]: $out"
chk row7.loopback-http-allowed 0 "$rc" "$out"

echo "== row7 worker binds loopback =="
sandbox "$NODE" -e "require('http').createServer((q,r)=>r.end('WORKER-OK\n')).listen($BIND_PORT,'127.0.0.1',()=>console.log('bound'))" > "$RUN/worker-bind.log" 2>&1 &
sleep 1.2
out=$(curl -s -m 2 "http://127.0.0.1:$BIND_PORT/"); rc=$?
echo "PARENT_TO_WORKER[$rc]: $out (bind_log=$(cat "$RUN/worker-bind.log"))"
chk row7.worker-bind-loopback 0 "$rc" "$out"

echo "== row8 unix domain socket (nerve bridge) =="
"$NODE" "$S0B/probes/net-uds.js" "$SOCK/nerve.sock" > "$RUN/net-uds.log" 2>&1 &
UDS_PID=$!
sleep 1
out=$(printf 'ping\n' | sandbox /usr/bin/nc -U -w 2 "$SOCK/nerve.sock"); rc=$?
echo "SANDBOX_UDS_NC[$rc]: $out"
# nc exit code is often non-zero even on success; grade on the response payload
case "$out" in *NERVE-OK:ping*) ures=0;; *) ures=1;; esac
chk row8.nerve-socket-allowed 0 "$ures" "$out"

# off-path socket: outside the allowed SOCKDIR -> denied
"$NODE" "$S0B/probes/net-uds.js" "$RUN/state/forbidden.sock" > "$RUN/net-forbidden.log" 2>&1 &
FORBID_PID=$!
sleep 1
out=$(printf 'x\n' | sandbox /usr/bin/nc -U -w 2 "$RUN/state/forbidden.sock"); rc=$?
echo "SANDBOX_OFFPATH_UDS[$rc]: $out"
chk row8.offpath-socket-denied 1 "$rc" "$out"

echo "== row9 arbitrary external network denied =="
# baseline: unconfined parent has external connectivity
presp=$(curl -s -m 5 -o /dev/null -w '%{http_code}' "http://example.com/"); prc=$?
echo "PARENT_EXTERNAL[$prc]: http=$presp"
chk row9.parent-external-baseline 0 "$prc" "http=$presp"
# DNS name resolution denied
out=$(sandbox /usr/bin/curl -s -m 5 "http://example.com/"); rc=$?
echo "SANDBOX_EXT_DNS[$rc]: $out"
if [ "$rc" -ne 0 ]; then chk row9.external-dns-denied 0 0 "rc=$rc"; else chk row9.external-dns-denied 1 "$rc" "$out"; fi
# raw IP outbound denied
out=$(sandbox /usr/bin/curl -s -m 5 "http://1.1.1.1/"); rc=$?
echo "SANDBOX_EXT_IP[$rc]: $out"
if [ "$rc" -ne 0 ]; then chk row9.external-ip-denied 0 0 "rc=$rc"; else chk row9.external-ip-denied 1 "$rc" "$out"; fi

cleanup; HTTP_PID=""; UDS_PID=""; FORBID_PID=""
