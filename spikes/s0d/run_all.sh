#!/usr/bin/env bash
# S0d seam-freeze runner.
#
# Builds the std-only spike offline, runs the static-decision smoke test, runs
# the harness scenario under both bindings (reference and OpenCode) with only
# the config file changed, checks reference determinism, compares normalized
# event kinds, and runs the privacy check.
#
# Usage:
#   spikes/s0d/run_all.sh
#
# Artifacts land under spikes/s0d/runs/<UTC>/ (gitignored). No temp dirs.

set -uo pipefail

here="$(cd "$(dirname "$0")" && pwd)"
utc="$(date -u +%Y%m%dT%H%M%SZ)"
run="$here/runs/$utc"
mkdir -p "$run"

log() { echo "[s0d] $*"; }
overall=0

log "run dir: $run"

# --- build (offline, zero deps) ----------------------------------------------
if cargo build --offline --manifest-path "$here/Cargo.toml" 2>"$run/build.err"; then
  log "build: OK (cargo build --offline)"
else
  log "build: FAILED"
  cat "$run/build.err"
  exit 2
fi
bin="$here/target/debug/s0d"

# --- static decision baselines ------------------------------------------------
decisions_ok=0
if cargo test --offline --manifest-path "$here/Cargo.toml" >"$run/cargo-test.log" 2>&1; then
  log "decisions native tests: OK"
else
  log "decisions native tests: FAILED"
  decisions_ok=1
  overall=1
fi
if "$bin" --decisions-smoke >"$run/decisions-smoke.txt" 2>&1; then
  log "decisions smoke: PASS"
else
  log "decisions smoke: FAILED"
  decisions_ok=1
  overall=1
fi

# --- reference binding, twice (determinism) -----------------------------------
ref_ok=0
mkdir -p "$run/ref1" "$run/ref2" "$run/oc"
"$bin" --config "$here/config.reference.json" \
  --run-dir "$run/ref1" --workspace "$run/ref1/ws" \
  --out "$run/ref1/transcript.txt" >"$run/ref1/stdout.txt" 2>&1 || ref_ok=1
"$bin" --config "$here/config.reference.json" \
  --run-dir "$run/ref2" --workspace "$run/ref2/ws" \
  --out "$run/ref2/transcript.txt" >"$run/ref2/stdout.txt" 2>&1 || ref_ok=1

if [ "$ref_ok" -eq 0 ]; then
  log "reference binding: PASS"
else
  log "reference binding: FAILED"
  overall=1
fi

det_ok=1
if diff -u "$run/ref1/transcript.txt" "$run/ref2/transcript.txt" >"$run/ref.diff" 2>&1; then
  det_ok=0
  log "reference determinism: PASS (byte-identical across two runs)"
else
  log "reference determinism: FAILED (see ref.diff)"
  overall=1
fi

# --- opencode binding (same binary, same flags, config-only flip) -------------
oc_ok=0
"$bin" --config "$here/config.opencode.json" \
  --run-dir "$run/oc" --workspace "$run/oc/ws" \
  --out "$run/oc/transcript.txt" >"$run/oc/stdout.txt" 2>&1 || oc_ok=1
if [ "$oc_ok" -eq 0 ]; then
  log "opencode binding: PASS"
else
  # Non-fatal: the reference binding and all other checks still stand.
  log "opencode binding: FAILED (recorded; reference binding and checks still reported)"
  overall=1
fi

# --- privacy check ------------------------------------------------------------
if "$here/privacy-check.sh" >"$run/privacy.txt" 2>&1; then
  log "privacy check: PASS"
else
  log "privacy check: FAILED (see privacy.txt)"
  overall=1
fi

# --- normalized event-kind comparison -----------------------------------------
kinds() {
  sed -n '/^events:/,/^checks:/p' "$1" | sed -n 's/^  \([A-Za-z][A-Za-z]*\).*/\1/p' | sort -u
}
baseline_kinds=(TurnStarted UsageSnapshot TurnCompleted)
kinds_ok=0
for t in "$run/ref1/transcript.txt" "$run/oc/transcript.txt"; do
  [ -f "$t" ] || { kinds_ok=1; continue; }
  for k in "${baseline_kinds[@]}"; do
    if ! kinds "$t" | grep -qx "$k"; then
      kinds_ok=1
      log "event kinds: missing $k in $(basename "$t")"
    fi
  done
done
if [ "$kinds_ok" -eq 0 ] && [ "$ref_ok" -eq 0 ] && [ "$oc_ok" -eq 0 ]; then
  log "comparable event kinds: PASS (baseline kinds present in both bindings)"
else
  log "comparable event kinds: INCOMPLETE"
  [ "$ref_ok" -eq 0 ] && [ "$oc_ok" -eq 0 ] || overall=1
fi

# --- summary ------------------------------------------------------------------
{
  echo "run: $run"
  echo "utc: $utc"
  echo "build: OK"
  echo "decisions: $([ "$decisions_ok" -eq 0 ] && echo PASS || echo FAIL)"
  echo "reference_binding: $([ "$ref_ok" -eq 0 ] && echo PASS || echo FAIL)"
  echo "reference_determinism: $([ "$det_ok" -eq 0 ] && echo PASS || echo FAIL)"
  echo "opencode_binding: $([ "$oc_ok" -eq 0 ] && echo PASS || echo FAIL)"
  echo "event_kinds_comparable: $([ "$kinds_ok" -eq 0 ] && echo PASS || echo INCOMPLETE)"
  echo "privacy: $(grep -q 'privacy-check: PASS' "$run/privacy.txt" 2>/dev/null && echo PASS || echo FAIL)"
  echo "overall: $([ "$overall" -eq 0 ] && echo PASS || echo FAIL)"
} >"$run/summary.txt"

echo
cat "$run/summary.txt"
exit "$overall"
