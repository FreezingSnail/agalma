# Hardening wave — design (pre-B0)

Bead: `agalma-zxi.1` (H0). Epic: `agalma-zxi` (M1.5). Inputs: M1 exit evidence
(`docs/m1-exit-evidence.md`) findings 1–4; model research
(`docs/research/*`). Orchestrator-authored; gates H1–H4.

## Scope

Make unattended batches faster and routing honest before B0. No architecture
changes; Tier-1 contracts unchanged unless noted.

## H1 — stall watchdog, provider health, park projection (`agalma-zxi.2`)

Problem: 8/10 sessions hung with zero tokens until a blind 600s deadline;
cancel then hit `SessionNotFound`; parked tasks show `in_progress` on the board.

Design:

- Poll loop (`poll_terminal`) tracks a progress fingerprint from
  `inspect_operation`: terminal state, usage/tokens/cost delta, operation
  status. No delta for `idle_timeout` (default 120s, env/flag override) and not
  terminal → cancel, classify attempt verdict `stalled`. Hard cap stays 600s.
- Session gone is terminal: `inspect_operation`/cancel returning
  session-not-found (404) or connection refused → immediate `stalled` verdict,
  no waiting for the deadline.
- Provider health: ledger event `provider_health` per stalled/failed attempt:
  `{model, binding, attempt, reason: idle|session_gone|error, idle_ms,
  tokens, wall_ms}`. Digest includes a per-run stall count.
- Retry decision: `RetryEscalate` static baseline treats `stalled` like a
  failed verdict (retry while attempts remain, else park); H3 may use
  provider-health history to pick the next candidate.
- Park projection: `parked` → bd status `blocked` + label `parked` + reason
  comment (not `in_progress`); reconcile covers the mapping idempotently.
  Update `docs/m1-workloop.md` projection sentence.
- Tests (scripted/reference harness): silent session → idle verdict with small
  override and `provider_health` record; session-gone fast-fail (no 120s wait);
  stall count in digest; park → blocked+label; reconcile idempotent; existing
  recovery/fencing/work tests green.

## H2 — xcrun/TMPDIR acceptance fix (`agalma-zxi.3`)

Symptom: sandboxed acceptance `git` (guard command) logged
`couldn't create cache file '/var/folders/.../T/xcrun_db-…' (EPERM)`.

Design: acceptance launch env must carry `TMPDIR` (and `HOME` where needed)
pointing at a writable dir under the verification checkout; reproduce first,
then minimal fix. If xcrun ignores `TMPDIR` (uses confstr), add the narrowest
profile allowance for the per-user darwin temp dir or set
`xcrun`-compatible env (`CLT_NO_ANALYTICS`/cache overrides) — decide
empirically, document. Regression test: git-based guard command under the
sandbox exits clean with no xcrun errors in captured output.

## H3 — model routing v1 (`agalma-zxi.4`)

Static, evaluation-ready routing over the researched catalog (prices from
`docs/research/opencode-models.md`, re-verified at implementation time).

- `ModelSpec` metadata table (code, not config): id, provider, input/output/
  cache price, context, strict-schema flag, notes. Cash vs quota
  (`opencode-go` allowance) recorded separately.
- Task class: derive from task `family` + phase role
  (`docs` class = family fix/feature with docs-only acceptance? M1 seed tasks
  are docs work; v1 mapping: builder for doc/prompt tasks → free `big-pickle`
  default; fix/feature builders → `deepseek-v4.1-flash` (fallback
  `glm-5.3-flash`, `mimo-v2.6-flash`); verifier/diagnosis → strict-schema tier
  (`claude-haiku-5-5`/`gpt-6-luna`) when a paid route is enabled, free
  otherwise; triage/decision JSON → static baseline, no model).
- Route selection is a recorded decision (`route.model`, static policy):
  eligible candidates from the table, chosen pinned for the attempt/operation
  (rebind rules unchanged), recorded in the attempt event + digest.
- Defaults stay free-only until the auth proxy exists: route lists include
  paid candidates but selection skips them unless `AGALMA_ALLOW_PAID=1`;
  `--model` still overrides.
- Digest/cost: per attempt model id + route, cash and quota fields; per-model
  aggregate helper (`agalma digest` summary or small query) for $/merge by
  model.
- Tests: deterministic selection/fallbacks per class; paid skipped without
  opt-in; attempt pins model; digest records model; no regression.

## H4 — batch rerun evidence (`agalma-zxi.5`)

Fresh scratch clone, same 4 seed tasks (plus optionally re-seed 2 to confirm
previously parked tasks now pass with a healthier model). Compare success
rate, wall time, $/merge vs M1.8 (50%, 102.9 min, $0), and watchdog
effectiveness (stall detection latency, provider_health counts). Evidence in
`docs/hardening-evidence.md`; keep `docs/m1-exit-evidence.md` as baseline
(link, don't rewrite).

## Waves

- W1: H1 `agalma-zxi.2` + H2 `agalma-zxi.3` (mostly disjoint: watchdog in
  phases/poll + ledger events; xcrun in verify launch env) — parallel.
- W2: H3 `agalma-zxi.4` (phases/work/decision) — single.
- W3: H4 `agalma-zxi.5` (batch) — single.

## Constraints

No new crates without approval; offline builds after first fetch; no commits
from workers; run dirs under `target/test-runs/`; live repo/bd untouched by
factory runs (scratch clone only).
