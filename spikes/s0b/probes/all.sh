#!/bin/sh
# S0b probe driver: fresh run dir, seed fixtures, run every probe, summarize.
# Permanent artifacts only under spikes/s0b. No writes outside run dirs.
set -u
S0B="$(cd "$(dirname "$0")/.." && pwd -P)"
REPO="$(cd "$S0B/../.." && pwd -P)"
RUN="${1:-$S0B/runs/$(date -u +%Y%m%dT%H%M%SZ)}"
mkdir -p "$RUN/attempt" "$RUN/protected" "$RUN/state" "$RUN/sockets" "$RUN/logs"
RUN="$(cd "$RUN" && pwd -P)"

# fixtures
echo 'PROTECTED TEST FIXTURE' > "$RUN/protected/guard_test.sh"
echo 'ledger secret'        > "$RUN/state/ledger.sqlite"
echo 'attempt scratch'      > "$RUN/attempt/scratch.txt"
echo '{"token":"DUMMY-SECRET"}' > "$RUN/state/auth.json"
ln -sfn "$(basename "$RUN")" "$S0B/runs/latest"

PIDS=""
for p in p01-file-access p02-descendants p03-build p04-network p05-failclosed p06-resources p07-authproxy-bridge p08-opencode-seatbelt; do
  echo "##### $p #####"
  "$S0B/probes/$p.sh" "$RUN" "$REPO" 2>&1 | tee "$RUN/logs/$p.log"
done

echo
echo "===== SUMMARY ====="
grep -h '^RESULT' "$RUN"/logs/*.log | sort
echo "----- counts -----"
grep -h '^RESULT' "$RUN"/logs/*.log | awk '{print $3}' | sort | uniq -c
echo "run=$RUN"
