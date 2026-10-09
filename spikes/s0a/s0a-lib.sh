#!/bin/sh
# S0a spike helpers. Source this file. No downloads, no writes outside $RUN.
# Usage: . spikes/s0a/s0a-lib.sh
set -u

S0A_ROOT="$(cd "$(dirname "$0")" && pwd)"
if [ -f "$S0A_ROOT/.last_run" ]; then
  RUN="${S0A_ROOT}/$(cat "$S0A_ROOT/.last_run" | sed 's#^spikes/s0a/##')"
fi
RUN="${RUN:?set RUN}"

OPENCODE_BIN="${OPENCODE_BIN:-/Users/connorfranc/.local/bin/opencode}"
S0A_HOST="${S0A_HOST:-127.0.0.1}"
S0A_PORT="${S0A_PORT:-39001}"
S0A_URL="http://${S0A_HOST}:${S0A_PORT}"
S0A_PASSWORD="${S0A_PASSWORD:-s0a-spike-password}"
CAP="$RUN/capture"

# Isolated XDG + tmp. Never touches the user's real opencode state.
s0a_env() {
  export XDG_CONFIG_HOME="$RUN/xcfg"
  export XDG_DATA_HOME="$RUN/xdata"
  export XDG_CACHE_HOME="$RUN/xcache"
  export XDG_STATE_HOME="$RUN/xstate"
  export TMPDIR="$RUN/tmp"
  export OPENCODE_PASSWORD="$S0A_PASSWORD"
  export OPENCODE_TEST_HOME="${OPENCODE_TEST_HOME:-$RUN/home}"
  mkdir -p "$OPENCODE_TEST_HOME"
}

# Bearer/basic curl against the spike server. Redacts the auth key from stdout.
s0a_curl() {
  curl -sS -u "opencode:${S0A_PASSWORD}" "$@"
}

# Redact the provider API key (and any token-ish value) from a stream on stdin.
s0a_redact() {
  KEY="$(jq -r '."opencode-go".key // empty' "$RUN/xdata/auth.json" 2>/dev/null)"
  if [ -n "${KEY:-}" ]; then
    sed -e "s#${KEY}#APIKEY_REDACTED#g"
  else
    cat
  fi
}

# Save a captured body to $CAP/<name> and print a redacted preview.
s0a_save() {
  name="$1"; shift
  out="$CAP/$name"
  "$@" | tee "$out.raw" | s0a_redact > "$out"
  rm -f "$out.raw"
  echo "saved: $out"
}
