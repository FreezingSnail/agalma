#!/bin/sh
# Live cockpit launcher for `make start`.
#
# Runs the conductor's continuous runtime, which supervises the sealed
# agalma-web-dashboard package and serves the real cockpit. Streams the
# runtime's stderr, announces the loopback URL, optionally opens it, forwards
# interactive signals to the runtime, and exits with the runtime's status. It
# starts no fixture server and claims no task itself: dispatch is the runtime's
# own queue behavior.
#
# Required environment:
#   AGALMA_BIN   conductor executable (the `agalma` binary).
#   REPO         origin repository and bd backlog.
#   PLUGIN_ROOT  sealed plugin root holding the dashboard package.
# Optional:
#   STATE_DIR        durable state directory (absent means the runtime's own
#                    resolution: AGALMA_STATE_DIR or its default).
#   CONTROL_PLUGIN   plugin id approved for factory.control/v1 (absent means no
#                    factory-control selector).
#   NO_OPEN          set to 1 to print the URL without opening it.

set -u

: "${AGALMA_BIN:?AGALMA_BIN is required}"
: "${REPO:?REPO is required}"
: "${PLUGIN_ROOT:?PLUGIN_ROOT is required}"

tmpdir=$(mktemp -d "${TMPDIR:-/tmp}/agalma-cockpit.XXXXXX") || {
    echo "cockpit-live: cannot create a temporary directory" >&2
    exit 1
}
fifo="$tmpdir/stderr"
if ! mkfifo "$fifo"; then
    echo "cockpit-live: cannot create a named pipe" >&2
    rm -rf "$tmpdir"
    exit 1
fi

child=
reader=

cleanup() {
    if [ -n "$reader" ]; then
        kill "$reader" 2>/dev/null || true
    fi
    rm -rf "$tmpdir"
}

forward() {
    if [ -n "$child" ]; then
        kill "-$1" "$child" 2>/dev/null || true
    fi
}

trap cleanup EXIT
trap 'forward INT' INT
trap 'forward TERM' TERM
trap 'forward HUP' HUP

set -- work --watch --repo "$REPO" --plugin-root "$PLUGIN_ROOT"
if [ -n "${STATE_DIR:-}" ]; then
    set -- "$@" --state-dir "$STATE_DIR"
fi
if [ -n "${CONTROL_PLUGIN:-}" ]; then
    set -- "$@" --factory-control-plugin "$CONTROL_PLUGIN"
fi

# The runtime writes its log lines, including `DASHBOARD_URL <url>`, to stderr.
# Route stderr through a named pipe so the launcher can stream each line and
# react to the URL line while the runtime keeps running in the foreground of
# this script's lifetime.
"$AGALMA_BIN" "$@" 2>"$fifo" &
child=$!

(
    announced=0
    while IFS= read -r line || [ -n "$line" ]; do
        printf '%s\n' "$line" >&2
        case $line in
            DASHBOARD_URL\ *)
                if [ "$announced" -eq 0 ]; then
                    announced=1
                    url=${line#DASHBOARD_URL }
                    printf 'AGALMA_COCKPIT_URL %s\n' "$url"
                    if [ "${NO_OPEN:-0}" != 1 ] &&
                        [ "$(uname -s)" = Darwin ] &&
                        command -v open >/dev/null 2>&1; then
                        open "$url" >/dev/null 2>&1 || true
                    fi
                fi
                ;;
        esac
    done <"$fifo"
) &
reader=$!

# Reap the runtime, retrying when an interactive signal interrupts `wait` while
# the runtime is still shutting down, and exit with the runtime's status.
status=0
while :; do
    wait "$child"
    status=$?
    if ! kill -0 "$child" 2>/dev/null; then
        break
    fi
done
wait "$reader" 2>/dev/null || true
exit "$status"
