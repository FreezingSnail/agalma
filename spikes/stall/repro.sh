#!/bin/sh
# spikes/stall/repro.sh — stall root-cause reproducer (agalma-csw.1)
#
# Reproduces the "free Zen session stalls with zero tokens; watchdog cancels at
# ~120s idle" report and classifies it. This runs a PLAIN local `opencode serve`
# (no sandbox, no provider proxy) so provider behaviour is separated from
# harness plumbing; the harness's sandbox+proxy path is analysed from the
# recorded batch artifacts (see docs/spikes/stall-root-cause.md).
#
# It shows, for a session that is demonstrably WORKING (the model issues several
# sequential shell tool calls; SSE streams reasoning/tool/step frames):
#   1. GET /api/session/{id}                  -> outcome absent, tokens 0/0 (stale until terminal)
#   2. GET /api/experimental/session/{id}/log -> only `log.synced` (no frames)
#   3. the adapter's ProgressFingerprint therefore never changes, so the idle
#      watchdog fires at IDLE_SECS while the session is progressing
#   4. reproducing the harness order (close_session DELETE, then cancel_operation
#      interrupt) yields `SessionNotFoundError` (404) though the server and
#      session were alive -> the 404 is a harness ordering artifact, not a
#      server-side session loss.
#
# Usage:  sh spikes/stall/repro.sh
#         IDLE_SECS=130 MODEL=opencode/big-pickle STEPS=4 STEP_SLEEP=35 sh spikes/stall/repro.sh
#         QUICK=1 sh spikes/stall/repro.sh     # short smoke
set -eu

ROOT="$(cd "$(dirname "$0")" && pwd)"
TS="$(date -u +%Y%m%dT%H%M%SZ)"
RUN="$ROOT/runs/stall-$TS"
mkdir -p "$RUN/xdg-config" "$RUN/xdg-data" "$RUN/xdg-cache" "$RUN/xdg-state" "$RUN/tmp" "$RUN/work"
CHECKOUT="$RUN/checkout"; mkdir -p "$CHECKOUT"
( cd "$CHECKOUT" && git init -q && git config user.email t@t && git config user.name t \
  && echo hi > README.md && git add -A && git commit -qm init )

PORT="${PORT:-$(( 40000 + ($$ % 20000) ))}"
PW="stalltest-$$"
BIN="${OPENCODE_BIN:-$(command -v opencode)}"
MODEL="${MODEL:-opencode/big-pickle}"
QUICK="${QUICK:-0}"
if [ "$QUICK" = "1" ]; then IDLE_SECS="${IDLE_SECS:-12}"; else IDLE_SECS="${IDLE_SECS:-130}"; fi
STEPS="${STEPS:-4}"; STEP_SLEEP="${STEP_SLEEP:-35}"; POLL="${POLL:-5}"

export XDG_CONFIG_HOME="$RUN/xdg-config" XDG_DATA_HOME="$RUN/xdg-data" \
  XDG_CACHE_HOME="$RUN/xdg-cache" XDG_STATE_HOME="$RUN/xdg-state" \
  TMPDIR="$RUN/tmp" HOME="$RUN/work" OPENCODE_SERVER_PASSWORD="$PW" \
  OPENCODE_CONFIG_DIR="$RUN/work" OPENCODE_MODELS_URL="http://127.0.0.1:1" \
  OPENCODE_DISABLE_AUTOUPDATE=1
export PATH="/opt/homebrew/bin:/usr/bin:/bin:/usr/sbin:/sbin:$HOME/.local/bin"

SRV=""; SSEPID=""
cleanup() { [ -n "$SRV" ] && kill "$SRV" 2>/dev/null; [ -n "$SSEPID" ] && kill "$SSEPID" 2>/dev/null; }
trap cleanup EXIT INT TERM

echo "# stall reproducer"
echo "# run dir : $RUN"
echo "# bin     : $BIN ($("$BIN" --version 2>/dev/null | tail -1))"
echo "# model   : $MODEL"
echo "# idle    : ${IDLE_SECS}s  steps=$STEPS step_sleep=${STEP_SLEEP}s  poll=${POLL}s  quick=$QUICK"

get()  { curl -s -m 8 -u "opencode:$PW" "http://127.0.0.1:$PORT$1"; }
code() { curl -s -m 8 -o /dev/null -w '%{http_code}' -u "opencode:$PW" "http://127.0.0.1:$PORT$1"; }

# Readiness: /api/info answers JSON once the listening API is up. A healthy boot
# takes ~1s; a cold XDG boot fetches the model catalog over the network and can
# hang for ~2 min, during which the API is not usable and sessions created in the
# window are permanently unpromptable. So bound readiness at 20s and, on a slow
# boot, relaunch with a fresh XDG (retry until a healthy boot).
start_server() {
  "$BIN" serve --hostname 127.0.0.1 --port "$PORT" >"$RUN/server.out" 2>"$RUN/server.err" &
  SRV=$!; echo "$SRV" > "$RUN/server.pid"
  i=0
  while [ "$i" -lt 80 ]; do
    if get /api/info | grep -q '"version":"'; then READY=1; return; fi
    kill -0 "$SRV" 2>/dev/null || { READY=0; return; }
    i=$((i+1)); sleep 0.25
  done
  READY=0
}

READY=0
for boot in 1 2 3 4 5; do
  start_server
  [ "$READY" = "1" ] && break
  echo "# boot attempt $boot slow (cold catalog fetch), relaunching"
  kill "$SRV" 2>/dev/null || true; sleep 1; kill -9 "$SRV" 2>/dev/null || true
  rm -rf "$RUN/xdg-config" "$RUN/xdg-data" "$RUN/xdg-cache" "$RUN/xdg-state" "$RUN/tmp"
  mkdir -p "$RUN/xdg-config" "$RUN/xdg-data" "$RUN/xdg-cache" "$RUN/xdg-state" "$RUN/tmp"
done
VER=$(get /api/info | sed -n 's/.*"version":"\([^"]*\)".*/\1/p')
echo "# server : pid=$SRV port=$PORT version=${VER:-unknown} ready=$READY boot_attempts=$boot"

PROVIDER="${MODEL%%/*}"; MID="${MODEL##*/}"

# read_events mirror: always TurnStarted; tool frames only from the durable log
# (which emits only log.synced -> none); UsageSnapshot only at terminal.
fingerprint() { # $1 session json, $2 log body
  out=$(printf '%s' "$1" | sed -n 's/.*"outcome":"\([^"]*\)".*/\1/p')
  state=Running
  [ "$out" = "succeeded" ] && state=Completed
  [ -n "$out" ] && [ "$out" != "succeeded" ] && state=Failed
  tools=$(printf '%s' "$2" | grep -c '"type":"[^"]*tool' || true)
  printf 'state=%s steps=%s' "$state" "$((1 + ${tools:-0}))"
}

# prompt: model must perform STEPS sequential shell tool calls, each waiting on
# the previous (drives a > IDLE_SECS turn with per-step progress).
printf '{"text":"Issue shell tool calls ONE AT A TIME, waiting for each result before the next. Do exactly this sequence: for n from 1 to %s run the shell command `sleep %s; echo step-$n` as its own tool call. After step %s, reply DONE. Never combine calls."}' \
  "$STEPS" "$STEP_SLEEP" "$STEPS" > "$RUN/prompt.json"

# OpenCode boots per-directory "location services" lazily; a session created or
# prompted before its location is booted is rejected (`schema rejection`) or the
# request blocks until boot. Self-heal: start SSE, create a session, prompt it,
# and recreate on failure until the turn is admitted.
curl -s -N -u "opencode:$PW" "http://127.0.0.1:$PORT/api/event" >"$RUN/sse.txt" 2>&1 &
SSEPID=$!

SID=""; PCODE=000; k=0; DEADLINE=$(( $(date +%s) + 90 ))
while [ "$(date +%s)" -lt "$DEADLINE" ]; do
  SESS=$(curl -s -m 30 -u "opencode:$PW" -H 'content-type: application/json' \
    -d "{\"title\":\"stall-repro\",\"model\":{\"id\":\"$MID\",\"providerID\":\"$PROVIDER\"},\"location\":{\"directory\":\"$CHECKOUT\"}}" \
    "http://127.0.0.1:$PORT/api/session")
  SID=$(printf '%s' "$SESS" | sed -n 's/.*"data":{"id":"\([^"]*\)".*/\1/p')
  if [ -z "$SID" ]; then
    echo "# create attempt $k: no session id: $(printf '%s' "$SESS" | cut -c1-120)"
    k=$((k+1)); sleep 2; continue
  fi
  PCODE=$(curl -s -m 30 -o "$RUN/prompt.resp" -w '%{http_code}' -u "opencode:$PW" \
    -H 'content-type: application/json' --data-binary @"$RUN/prompt.json" \
    "http://127.0.0.1:$PORT/api/session/$SID/prompt" || true)
  [ "$PCODE" = "200" ] && break
  echo "# prompt attempt $k rejected: $(cut -c1-120 "$RUN/prompt.resp")"
  curl -s -m 5 -u "opencode:$PW" -X DELETE "http://127.0.0.1:$PORT/api/session/$SID" >/dev/null 2>&1 || true
  SID=""; k=$((k+1)); sleep 2
done
echo "# session: $SID (prompt http=$PCODE after $k create attempts)"

TL="$RUN/timeline.tsv"
printf 't_s\talive\tfingerprint\tsession_tokens\tlog_seq\tmsg_tool_parts\ttool_states\tsse_steps\tsse_tools\tsse_reasoning\n' >"$TL"
T=0; LASTFP=""
while :; do
  SJ=$(get "/api/session/$SID")
  LG=$(get "/api/experimental/session/$SID/log")
  MS=$(get "/api/session/$SID/message")
  FP=$(fingerprint "$SJ" "$LG")
  TOK=$(printf '%s' "$SJ" | sed -n 's/.*"tokens":{"input":\([0-9]*\),"output":\([0-9]*\).*/\1\/\2/p' | head -1)
  SEQ=$(printf '%s' "$LG" | sed -n 's/.*"seq":\([0-9]*\).*/\1/p' | head -1)
  NTOOL=$(printf '%s' "$MS" | grep -o '"type":"tool"' | grep -c . || true)
  TS=$(printf '%s' "$MS" | grep -o '"status":"[a-z_]*"' | sort -u | tr '\n' ',' | sed 's/,$//')
  SS=$(grep -c 'session.step.started' "$RUN/sse.txt" || true)
  ST=$(grep -c 'session.tool.called' "$RUN/sse.txt" || true)
  SR=$(grep -c 'session.reasoning.delta' "$RUN/sse.txt" || true)
  if kill -0 "$SRV" 2>/dev/null; then ALIVE=yes; else ALIVE=no; fi
  printf '%s\t%s\t%s\t%s\t%s\t%s\t%s\t%s\t%s\t%s\n' \
    "$T" "$ALIVE" "$FP" "${TOK:-?}" "${SEQ:-?}" "${NTOOL:-0}" "${TS:-none}" "${SS:-0}" "${ST:-0}" "${SR:-0}" >>"$TL"
  [ -z "$LASTFP" ] && LASTFP="$FP"
  echo "t+${T}s alive=$ALIVE [$FP] tokens=${TOK:-?} log_seq=${SEQ:-?} tool_parts=${NTOOL:-0} tool_state=${TS:-none} | SSE steps=${SS:-0} tools=${ST:-0} reasoning=${SR:-0}"
  if printf '%s' "$SJ" | grep -q '"outcome"'; then echo "# session reached terminal outcome"; break; fi
  [ "$T" -ge "$IDLE_SECS" ] && break
  [ "$QUICK" = "1" ] && [ "$T" -ge 10 ] && break
  sleep "$POLL"; T=$((T+POLL))
done
kill "$SSEPID" 2>/dev/null || true

echo
echo "# --- adapter verdict emulation (phases.rs progress_fingerprint) ---"
echo "# fingerprint at t0: $LASTFP ; idle bound ${IDLE_SECS}s"
echo "# distinct fingerprints observed while running:"
cut -f3 "$TL" | tail -n +2 | sort -u | sed 's/^/#   /'

echo
echo "# --- harness cancel ordering (close_session DELETE, then cancel_operation interrupt) ---"
echo "# GET session before cancel:  $(code "/api/session/$SID")"
echo "# DELETE (close_session):     $(curl -s -o /dev/null -w '%{http_code}' -u "opencode:$PW" -X DELETE "http://127.0.0.1:$PORT/api/session/$SID")"
echo "# POST interrupt (cancel):    $(curl -s -w ' [%{http_code}]' -u "opencode:$PW" -X POST "http://127.0.0.1:$PORT/api/session/$SID/interrupt")"

# control: interrupt a live session WITHOUT closing it first -> the server and
# session exist, so the interrupt is accepted (not 404).
SESS2=$(curl -s -u "opencode:$PW" -H 'content-type: application/json' \
  -d "{\"title\":\"stall-control\",\"model\":{\"id\":\"$MID\",\"providerID\":\"$PROVIDER\"},\"location\":{\"directory\":\"$CHECKOUT\"}}" \
  "http://127.0.0.1:$PORT/api/session")
SID2=$(printf '%s' "$SESS2" | sed -n 's/.*"data":{"id":"\([^"]*\)".*/\1/p')
curl -s -u "opencode:$PW" -H 'content-type: application/json' \
  -d '{"text":"Reply with exactly AGALMA_OK. Do not use any tools."}' \
  "http://127.0.0.1:$PORT/api/session/$SID2/prompt" >/dev/null
sleep 3
echo "# control GET live session:   $(code "/api/session/$SID2")"
echo "# control interrupt-before-close: $(curl -s -w ' [%{http_code}]' -u "opencode:$PW" -X POST "http://127.0.0.1:$PORT/api/session/$SID2/interrupt")"

echo
echo "# --- teardown ---"
kill "$SRV" 2>/dev/null || true
sleep 1; kill -9 "$SRV" 2>/dev/null || true
if kill -0 "$SRV" 2>/dev/null; then echo "# server alive after teardown: yes"; else echo "# server alive after teardown: no"; fi
echo "# non-INFO server log lines:"; grep -vE "level=INFO" "$RUN/xdg-data/opencode/log/opencode.log" 2>/dev/null | sed 's/^/#   /' | head -5 || true
echo "# done. evidence: $RUN"
