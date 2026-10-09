#!/bin/sh
# Start the isolated S0a OpenCode server on 127.0.0.1:39001.
# Foreground; redirect with nohup/& from the caller. No downloads.
set -u
S0A_ROOT="$(cd "$(dirname "$0")" && pwd)"
. "$S0A_ROOT/s0a-lib.sh"
s0a_env
exec "$OPENCODE_BIN" serve --hostname "$S0A_HOST" --port "$S0A_PORT" --log-level info
