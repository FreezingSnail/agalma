# M1.8 — seeded batch exit evidence

Bead: `agalma-52k.9` (M1.8). Base HEAD: `726fd6e`. Date: 2026-10-10. M1 exit run:
the queue-driven `agalma work` loop processing real repo tasks end to end.

Environment:

- Scratch target (never the live repo): `git clone --no-hardlinks` of agalma at
  `726fd6e` into `target/test-runs/m1-batch/run-1/target`, with an isolated bd
  store (`.beads` in the clone). All merges, tags, and bd writes happened only in
  the clone; the live repo has no commits or pushes.
- Harness `opencode@opencode-adapter/v1`, free model
  `opencode/mimo-v2.6-flash-free` via the parent-owned loopback egress proxy
  (`AGALMA_PROXY_ALLOW` default `opencode.ai`). Turn timeout 600 s, verify
  timeout 900 s, per-task `budget_usd: 2.00`, `max_attempts: 2`.
- Command:
  `cargo run --offline -p agalma-conductor -- work --repo <clone> --max-tasks 4 --state-dir <state> --harness opencode`
  (live mode; no `AGALMA_ACTIVITY_MODE`).

## Seed tasks

Four real data/docs bd issues, each carrying the fenced `directive/v0` block
(`target: agalma`, `ref: main`, deterministic shell `acceptance`, `budget_usd:
2.00`, `priority: normal`, `max_attempts: 2`). Acceptance runs inside the
pristine sandboxed verification checkout and needs no network. Every task ends
with a scope-guard command asserting the candidate diff touches no `Cargo.toml`
and no crate `src/**/*.rs`:

```
sh -c '! git diff --name-only HEAD~1 HEAD | grep -qE "Cargo\.toml$|/src/.*\.rs$"'
```

| # | Issue | Family | Title (abridged) |
|---|-------|--------|------------------|
| 1 | `agalma-8yr` | fix | README roles/loop drift fix (verifier role; checkout/integrate in loop) |
| 2 | `agalma-9cu` | feature | new `docs/task-authoring.md` describing the fenced block |
| 3 | `agalma-vc6` | fix | `crates/agalma-conductor/prompts/builder.md` acceptance/no-test-edits guidance |
| 4 | `agalma-on7` | feature | new `docs/conventions.md` phase-handoff rules |

Triage order (all normal priority, id tiebreak): `8yr` → `9cu` → `on7` → `vc6`.

## Batch results

| Issue | Exec | Family | Attempts | Verdict | Failing acceptance (attempt 1 / attempt 2) | Digest |
|-------|------|--------|----------|---------|--------------------------------------------|--------|
| `agalma-8yr` | `exec:agalma-8yr:1` | fix | 2 | **parked** | 3/4 (`grep verifier README.md`; `! grep tester`; `grep checkout && integrate`); same on attempt 2 | `state/runs/exec-agalma-8yr-1/artifacts/digest.json` |
| `agalma-9cu` | `exec:agalma-9cu:1` | feature | 1 | **done** | none (4/4 pass) | `state/runs/exec-agalma-9cu-1/artifacts/digest.json` |
| `agalma-on7` | `exec:agalma-on7:1` | feature | 1 | **done** | none (4/4 pass) | `state/runs/exec-agalma-on7-1/artifacts/digest.json` |
| `agalma-vc6` | `exec:agalma-vc6:1` | fix | 2 | **parked** | 2/3 (`grep "acceptance commands"`; `grep "do not edit the tests"`); same on attempt 2 | `state/runs/exec-agalma-vc6-1/artifacts/digest.json` |

Batch wall: `00:09:20Z → 01:52:12Z` = **6172 s (102.9 min)**, `EXIT=0`, all four
tasks processed (`--max-tasks 4`).

Terminal evidence in the clone:

- Merge on green + CAS + tag: `main` advanced `cc6d021 → 423fa9e → 345b743`;
  tags `m1/exec-agalma-9cu-1` (`423fa9e`) and `m1/exec-agalma-on7-1`
  (`345b743`), each pointing at merged `main`.
- Done tasks: bd `closed`, digest comment + `phase=done` projection recorded.
- Parked tasks: bd `in_progress` + label `parked` + `phase=parked`,
  `digest … verdict=red`, and `parked: max_attempts (2) exhausted …` comments;
  `artifacts/postmortem.md` written for both.
- Every execution has a persisted digest (ledger `digests` row +
  `artifacts/digest.json`).

## Metrics (M1 exit criteria)

- **Success rate: 2/4 = 50%** (both successes on the first attempt; both
  failures parked after exhausting 2 attempts).
- **$/merge: $0.00** — total recorded cost `0.000000 USD` over 2 merges. Free
  model; successful turns recorded 41 970 tokens in / 3 361 out at $0.
- **Wall time per task** (digest `wall_ms`, the sum of recorded usage windows):

  | Exec | `wall_ms` | ≈ |
  |------|-----------|---|
  | `exec:agalma-9cu:1` (done) | 407 786 | 6.8 min |
  | `exec:agalma-on7:1` (done) | 297 702 | 5.0 min |
  | `exec:agalma-8yr:1` (parked) | 2 470 897 | 41.2 min |
  | `exec:agalma-vc6:1` (parked) | 2 474 727 | 41.2 min |

  Batch wall **102.9 min**; productive model time ≈ 11.8 min, stall/timeout
  time ≈ 82 min (8 harness sessions that ended with zero recorded tokens after
  the 600 s deadline).
- Acceptance: 4 commands × 4 tasks executed in pristine verification checkouts
  (rerun on retries); each ≤ ~1.6 s (the guard command's `git` launch dominates).

## Fencing and rebind evidence (M1 exit clauses)

Deterministic suites (unchanged by this bead; regression below re-runs them):

- **Cannot merge twice** — `crates/agalma-conductor/tests/fencing.rs`:
  `duplicate_integrate_delivery_merges_once`,
  `stale_lease_operation_is_rejected`,
  `reclaim_single_winner_and_merged_effect_completes`.
- **Projection reconcile** — `fencing.rs`:
  `projection_reconcile_after_crash_is_idempotent`; the batch boot logged
  `reconcile: checked=0 fixed=0 stale=0 skipped=0`.
- **Staged harness rebind / pinned recovery** —
  `crates/agalma-conductor/tests/rebind.rs`:
  `red_reference_fail_then_rebind_reference_completes`,
  `recovery_uses_pinned_binding_not_flipped_config`,
  `new_generation_after_completion_uses_new_binding`,
  `bind_gate_rejects_missing_required_and_falls_back_for_optional`,
  `binary_work_harness_flag_and_status_binding`.
- Batch recorded `harness=opencode@opencode-adapter/v1` per execution (binding
  pinned at attempt start; `agalma status` reports it).

## Deviations / open items

1. **Batch exceeded the 60-minute timebox (102.9 min).** The batch was launched
   as one background command and driven to completion by the conductor; it was
   not stopped at 60 min (no partial-run interrupt). Recorded honestly rather
   than trimmed.
2. **Free-model stalls dominate wall time.** 8 of 10 harness build/verifier
   sessions ended with zero recorded tokens after the 600 s deadline; the
   interrupt then returned `SessionNotFoundError` (server-side session already
   gone). The conductor behaved correctly — timeout → advance to verify →
   recorded `retry.escalate` (`retry` then `park`) → bound → park — but a fixed
   600 s deadline cannot distinguish stalled from slow. Needs a token/step
   idle watchdog and provider-health signal (follow-up hardening bead).
3. **Timeout attempts record no usage.** Because a timed-out build never reaches
   a terminal turn, no `usage` event is written; the parked digests show
   `tokens=0/0`. Cost is honest ($0) but token accounting is incomplete for
   stalled attempts (measurement gap).
4. **Verifier fallback.** Both live verifier sessions timed out and produced no
   diagnosis file; the conductor wrote its structured fallback
   `artifacts/diagnosis@<n>.md` (confidence 0.0) which the retry prompt then
   referenced. No crash; honest artifact.
5. **Model quality, not loop mechanics.** `vc6` produced empty diffs on both
   attempts; `8yr` attempt 1 wrote an unrelated
   `fixtures/target-template/Cargo.lock` (a file outside the intended docs
   scope, but not `Cargo.toml`/`src/**`, so the guard admitted it). Verify runs
   before merge, so neither reached `main`.
6. **Scope guard is coarse.** The `Cargo.toml$|/src/.*\.rs$` guard admits other
   non-doc edits (e.g. the stray lockfile). The M1 ladder was still respected
   for the two merges (only `docs/**` landed).
7. **Sandboxed `git` xcrun noise.** The guard command launched `git` under
   Seatbelt and macOS `xcrun` could not create its cache under
   `/var/folders/...` (`Operation not permitted`); the command still exited
   with the correct status. Follow-up: allow/redirect the xcrun cache dir
   (TMPDIR) in the verify launch.
8. **Setup side effect, reverted.** The first `bd init` attempt walked up to the
   live repo's `.beads` and aborted; a follow-up init appended beads ignore
   lines to the live `.gitignore`. This was reverted with
   `git checkout -- .gitignore`; no live commits, pushes, or bd issue writes
   occurred. A separate concurrent writer had left an untracked
   `docs/m1-exit-evidence.md` (overwritten here with this run's data) and
   `docs/research/` (left untouched).

## Open items carried

- Stall watchdog + provider-health accounting (deviation 2/3).
- xcrun cache / TMPDIR in sandboxed acceptance (deviation 7).
- Prefer a stronger model tier via the provider-auth proxy before M3 budgets.
- Park projection stays `in_progress` + `parked` label — confirm board
  semantics before M2 intake.
- `OPENCODE_CONFIG_DIR` discovery workaround; mach-lookup narrowing; durable
  session-log reconciliation.
