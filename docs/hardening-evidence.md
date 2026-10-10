# Hardening evidence — batch rerun after H1–H3

Bead: `agalma-zxi.5` (H4). Base HEAD: `3b63e1b` (H1–H3 landed). Date:
2026-10-10. Baseline: `docs/m1-exit-evidence.md` (M1.8).

Setup: fresh scratch clone `target/test-runs/hardening-batch/run-1/target` at
`3b63e1b` + isolated `bd` store; same 4 tasks re-seeded with exact M1.8
wording (`agalma-cvr`=seed-1, `agalma-hhn`=seed-2, `agalma-rxb`=seed-3,
`agalma-9u8`=seed-4). Run: `agalma work --max-tasks 4`, no `--model`
(routing default `opencode/big-pickle`, paid gated off), idle watchdog 120s.

## Results

| Task | Attempts | Verdict | Stalls | Wall | Change |
|---|---|---|---|---|---|
| seed-3 builder prompt (`rxb`) | 2 (stall, then green) | **done** | 1 | 237.2s | `prompts/builder.md` +9/-2 |
| seed-1 README (`cvr`) | 2 | parked (stall×2) | 2 | 241.9s | none |
| seed-2 task-authoring (`hhn`) | 2 | parked (stall×2) | 2 | 241.4s | none |
| seed-4 conventions (`9u8`) | 2 | parked (stall×2) | 2 | 241.6s | none |

Batch wall: `03:56:27Z → 04:12:47Z` = **16.3 min** (baseline 102.9 min, 6.3×).

## Metrics vs M1.8

| Metric | M1.8 baseline | Hardening run |
|---|---|---|
| Success rate | 2/4 (50%) | 1/4 (25%) |
| Batch wall | 102.9 min | **16.3 min** |
| Stall cost per attempt | ~617s | **~120.3s** (5.1× less) |
| Stalls detected | none (blind timeout) | 7 (`idle_ms` 120.2–120.4s) |
| $/merge | $0.00 | $0.00 |
| Provider evidence | none | `provider_health` events (reason `idle`, model, idle_ms) |
| Park projection | `in_progress` + label | **`blocked` + `parked` label** (verified live) |
| Model/route recorded | no | `opencode/big-pickle`, route `builder:fix|feature`, stalls in digest |

Watchdog, session-gone fast-fail, park projection, routing metadata and
provider-health recording all behaved as designed. The success-rate drop is
provider behaviour, not loop logic: every failed build was a zero-token idle
session; cancel returns `SessionNotFound`; one task succeeded on the retry,
proving the model can work but is flaky.

## Findings

1. **Free Zen sessions stall with zero tokens** (both `big-pickle` now and
   `mimo-v2.6-flash-free` in M1.8). No 429/quota errors; sessions go silent at
   creation/first turn and disappear. Watchdog converts a 600s loss into 120s;
   the remaining lever is a working paid route (needs the provider-auth proxy)
   or another free candidate — and possibly an adapter/projection check
   (S0a: durable session log emitted only `log.synced`; a server-side
   completion may be invisible to projection, which would look identical to a
   stall). Distinguish provider stall vs projection gap next.
2. **Merged change verified on `main`**: both acceptance checks pass on the
   `main` ref; scope guard passed (prompt file only).
3. **Origin worktree caveat**: CAS `update-ref` advances the origin's `main`
   ref but does not update a checked-out worktree; re-running acceptance
   against the origin working tree shows stale files. Conductor verification
   uses the pristine candidate checkout, so verdicts are correct; note for M2
   (origin checkouts / merge queue must sync the worktree or use a bare
   origin).
4. Scope guard and pristine verification unchanged; workspace 39 suites green
   at `3b63e1b` + H4 (no code changes in H4).

## Open items

- Paid-route path (provider-auth proxy) to escape free-model stalls; measure
  $/merge then.
- Stall root cause: adapter/session projection vs provider — inspect opencode
  server-side state after a stall before blaming the provider.
- Origin worktree sync for local-path integration.
- Parked `blocked` projection is now live; downstream intake must not treat
  `blocked` as ready (bd `list` filters already exclude it).
