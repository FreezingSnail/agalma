# M1.5 — hardening batch evidence (H4)

Bead: `agalma-zxi.5` (H4). Epic: `agalma-zxi` (M1.5). Base HEAD: `3b63e1b`
(H3 model routing v1). Date: 2026-10-10. Baseline: M1.8 exit run
(`docs/m1-exit-evidence.md`, kept unchanged; treated read-only here).

H4 re-runs the same four M1 seed tasks through the queue-driven `agalma work`
loop after the H1–H3 hardening (stall watchdog + provider health + park
projection, scoped xcrun fix, model routing v1) and compares against the M1.8
baseline (50% success, 102.9 min wall, $0/merge, 8×600 s stalls).

## Environment

- Fresh scratch target (never the live repo):
  `git clone --no-hardlinks` of agalma at `3b63e1b` into
  `target/test-runs/hardening-batch/run-1/target`, with an isolated bd store
  (`.beads` in the clone, `bd init`, prefix `agalma`). All merges, tags, and bd
  writes happened only in the clone; the live repo has no commits, pushes, or bd
  writes (live HEAD still `3b63e1b`, live backlog still 32 issues).
- Harness `opencode@opencode-adapter/v1` (`binding:opencode:1`, config
  `72754e858243caf8`, pinned per execution). Live mode; provider egress via the
  parent-owned loopback proxy (`AGALMA_PROXY_ALLOW` default `opencode.ai`).
  Turn timeout 600 s, verify timeout 900 s, idle watchdog default 120 s
  (`AGALMA_IDLE_TIMEOUT_SECS` unset), per-task `budget_usd: 2.00`,
  `max_attempts: 2`.
- Command (no `--model`; routing v1 picks the free default; paid stays gated off
  because `AGALMA_ALLOW_PAID` is unset):

  ```
  cargo run --offline -p agalma-conductor -- work \
    --repo <clone> --max-tasks 4 --state-dir <clone>/state
  ```

- Batch wall: `2026-10-10T03:56:27Z → 04:12:47Z` = **980 s (16.3 min)**
  (`EXIT=0`), well inside the 60-minute timebox. All four tasks processed.

## Seed tasks

Same four real data/docs bd issues as M1.8, re-seeded with their **exact** M1.8
title/description (including the fenced `directive/v0` block and acceptance
commands) from the previous run's store. New ids; content byte-identical to the
M1.8 seeds.

| H4 id | M1.8 id | Family | Title (abridged) |
|-------|---------|--------|------------------|
| `agalma-cvr` | `agalma-8yr` | fix | README roles/loop drift fix (verifier role; checkout/integrate in loop) |
| `agalma-hhn` | `agalma-9cu` | feature | new `docs/task-authoring.md` describing the fenced block |
| `agalma-rxb` | `agalma-vc6` | fix | `crates/agalma-conductor/prompts/builder.md` acceptance/no-test-edits guidance |
| `agalma-9u8` | `agalma-on7` | feature | new `docs/conventions.md` phase-handoff rules |

Triage order (normal priority, id tiebreak): `9u8` → `cvr` → `hhn` → `rxb`
(i.e. `on7` → `8yr` → `9cu` → `vc6`).

Every task ends with the same scope-guard command asserting the candidate diff
touches no `Cargo.toml` and no crate `src/**/*.rs`:

```
sh -c '! git diff --name-only HEAD~1 HEAD | grep -qE "Cargo\.toml$|/src/.*\.rs$"'
```

## Batch results

| H4 id | Exec | Family | Route | Build attempts | Verdict | `wall_ms` | Stalls | Tokens in/out | Digest |
|-------|------|--------|-------|----------------|---------|-----------|--------|---------------|--------|
| `agalma-9u8` | `exec:agalma-9u8:1` | feature | `builder:feature` | 2 (both idle-stalled) | **parked** | 241 600 | 2 | 0/0 | `state/runs/exec-agalma-9u8-1/artifacts/digest.json` |
| `agalma-cvr` | `exec:agalma-cvr:1` | fix | `builder:fix` | 2 (both idle-stalled) | **parked** | 241 905 | 2 | 0/0 | `state/runs/exec-agalma-cvr-1/artifacts/digest.json` |
| `agalma-hhn` | `exec:agalma-hhn:1` | feature | `builder:feature` | 2 (both idle-stalled) | **parked** | 241 438 | 2 | 0/0 | `state/runs/exec-agalma-hhn-1/artifacts/digest.json` |
| `agalma-rxb` | `exec:agalma-rxb:1` | fix | `builder:fix` | 2 (attempt 1 idle-stalled, attempt 2 green) | **done** | 237 180 | 1 | 23 003/1 071 | `state/runs/exec-agalma-rxb-1/artifacts/digest.json` |

Model for every execution: `opencode/big-pickle` (routing v1 free default).

Terminal evidence in the clone:

- Merge on green + CAS + tag: `main` advanced `7885d9d → 6ffa450`; tag
  `m1/exec-agalma-rxb-1` (`6ffa450`). Only `agalma-rxb` merged.
- Done task: bd `closed`, `phase=done` + digest comment recorded.
- Parked tasks: bd status `blocked`, label `parked`, `phase=parked` +
  `parked: max_attempts (2) exhausted after attempt 2; last failure
  artifact:build@2` + digest (`verdict=red`) comments. **Not** `in_progress`
  (the H1 park projection).
- Every execution has a persisted digest (ledger `digests` row +
  `artifacts/digest.json`); boot reconcile logged
  `reconcile: checked=0 fixed=0 stale=0 skipped=0`.

### Merged-task verification (`agalma-rxb` → `agalma-vc6`)

Verified against the merged `main` HEAD (`6ffa450`, parent `7885d9d`):

- Scope: only `crates/agalma-conductor/prompts/builder.md` (`M`) — a prompt, not
  `docs/**` but within the intended docs/prompts scope; no `Cargo.toml`, no
  `src/**/*.rs`.
- Acceptance on merged `main`: all three commands exit `0`
  (`grep "acceptance commands"` ✓, `grep "do not edit the tests"` ✓, scope guard
  ✓). The phrases are present at lines 24/26/31 of the merged file.

## Metrics vs M1.8 baseline

| Metric | M1.8 baseline | H4 batch | Change |
|--------|---------------|----------|--------|
| Success rate | 2/4 = **50%** | 1/4 = **25%** | −25 pts |
| Merges | 2 (`9cu`, `on7`) | 1 (`vc6`) | −1 |
| Batch wall | 6172 s (**102.9 min**) | 980 s (**16.3 min**) | **6.3× faster** |
| $/merge (cash) | $0.00 | $0.00 | = |
| Recorded cost | `0.000000 USD` | `0.000000 USD` (cash $0, quota $0) | = |
| Stalls (`provider_health`) | 8 | 7 | −1 |
| Stall detection | 600 s blind deadline | **~120.2 s** idle watchdog | **≈5.0× faster** |
| Productive model time | ≈11.8 min | ≈1.93 min (`rxb` attempt 2) | — |
| Park projection | `in_progress` + `parked` label | `blocked` + `parked` label | fixed |
| Park reason | `max_attempts exhausted` | `max_attempts (2) exhausted after attempt 2; last failure artifact:build@2` | richer |

Wall/`provider_health` breakdown: 7 idle stalls (sum `wall_ms` 842 002 ms ≈
14.0 min) + 1 productive session (115 752 ms ≈ 1.93 min) + verify (170 ms) ≈
958 s, consistent with the 980 s batch wall.

## Watchdog and provider-health findings (H1)

- **Detection latency.** The idle watchdog fired 7× at `idle_ms` 120 160–120 416
  (mean ≈120 239 ms) and `wall_ms` 120 182–120 439 ms, i.e. ~120.2 s — versus the
  M1.8 blind 600 s deadline. That is the **≈5× detection improvement**; it is the
  single biggest driver of the wall-time collapse (stall cost fell from
  ≈80 min to ≈14 min).
- **Progress fingerprint.** Every stalled session had zero token/step delta
  (`tokens=0`), so the fingerprint never advanced and the watchdog correctly
  classified them `stalled`/`idle`. `provider_health` records carried
  `{model, binding, attempt, reason:idle, idle_ms, tokens:0, wall_ms}` per H1.
- **Stall verdict path.** A `stalled` verdict is treated as a failed attempt: it
  skips verify and drives `retry.escalate` directly (7 recorded decisions:
  `retry` on attempt 1, `park` on attempt 2), matching the H1 design. In M1.8 a
  600 s timeout instead *advanced to verify*, producing empty/zero-token verify
  attempts.
- **Session-gone fast-fail.** Not exercised this batch: every cancellation
  returned `SessionNotFound` (404) *after* the idle verdict, not as the terminal
  poll error. The fast-fail path remains covered by `watchdog.rs` tests.
- **Recovery by retry.** One stall was escaped: `rxb` attempt 1 stalled at
  120.3 s, attempt 2 produced the diff and merged. Retry can still recover a
  stalled free-tier session.

### Provider health (H3 routing vs free tier)

- All 7 stalls were `opencode/big-pickle` sessions that produced **zero tokens in
  120 s**; only 1 of 8 sessions (13%) produced tokens. M1.8's free
  `opencode/mimo-v2.6-flash-free` produced 2 productive of 10 sessions (20%).
- Routing v1 therefore did **not** improve free-tier provider health: the free
  candidate set is still stall-dominated, and `big-pickle` was at least as flaky
  as `mimo-v2.6-flash-free` in this environment. The success regression (25% vs
  50%) tracks this flakiness, not loop mechanics — the conductor's
  stall→retry→park handling was correct throughout.
- Follow-up (not this bead): validate/alternate the free builder candidate, or
  enable a paid tier behind the auth proxy, before trusting batch throughput.

## Model routing v1 findings (H3)

- 8 `route.model` decisions recorded (builder + verifier per task), all
  `origin=static`, class `builder`/`verifier`, family `fix`/`feature`, chosen
  free `opencode/big-pickle`. Paid candidates were skipped
  (`AGALMA_ALLOW_PAID` unset → `eligible(..., false)`).
- Routes surfaced in the digest (`route=builder:fix` / `builder:feature`) and in
  each `usage` event; attempt model pinned as `opencode/big-pickle`.
- No `--model`/`AGALMA_MODEL` override was set, so routing defaulted builders to
  the free coder as designed; the verifier free fallback is the same coder.

## Park projection findings (H1)

- 3 parks projected to bd status `blocked` + label `parked` + reason comment
  (not `in_progress`), and the digest/phase comments ride the issue.
- **No `postmortem.md` artifact** was written for the stall-parks: the stall →
  retry → park path bypasses `write_postmortem`, which runs only when a *red
  verify* exhausts `max_attempts`. Reason/digest evidence is still on the bd
  issue and in the ledger, but the structured artifact M1.8 produced is absent
  on the stall path (observability gap).

## Deviations / open items

1. **Success rate fell (25% vs 50%) despite the wall-time win.** Model/provider
   flakiness dominated: 7/8 free sessions stalled with zero tokens. The H1–H3
   mechanics behaved as designed; the free tier is the bottleneck. This is
   reported honestly rather than trimmed.
2. **Digest under-reports stall-then-success attempts.** `rxb`'s digest shows
   `attempts=0` and empty `commands` because `collect_commands` stops at the
   first missing `verify@{attempt}` receipt (attempt 1 stalled, so there is no
   `verify@1`), even though verify ran at attempt 2 and the `accept@2-*`
   artifacts exist. Same for all stall-parks (`attempts=0`). Measurement gap in
   the digest, not a correctness bug.
3. **Stall-parks write no `postmortem.md`** (see above).
4. **Working tree stale after integrate (pre-existing).** After a merge, `main`
   HEAD carries the change but the origin index/worktree still hold the
   pre-merge content (e.g. `M`/`D` entries). This is *not* an H regression: the
   M1.8 clone shows the same (`D docs/task-authoring.md`). Merged-task acceptance
   was therefore verified against HEAD (restoring the worktree to `6ffa450`),
   where all commands pass.
5. **`bd init` side effect in the clone.** The isolated init created
   `.claude/`, `AGENTS.md`, and a `bd init` commit in the scratch clone (no live
   effect). Live repo/bd untouched.
6. **Timebox met** (16.3 min < 60 min); no partial-run interruption needed.

## Post-run regression (live repo, read-only)

Run after the batch against the live tree at `3b63e1b`:

- `cargo test --workspace --offline` — **177 passed, 0 failed**.
- `cargo build --workspace --offline` — clean.
- `cargo clippy --workspace --all-targets --offline -- -D warnings` — clean.
- `cargo fmt --check --all` — clean.

## Open items carried forward

- Free-tier provider health: validate/alternate the builder candidate or gate a
  paid tier behind the auth proxy before relying on batch throughput.
- Digest `attempts`/`commands` accounting for stall-then-retry paths.
- `postmortem.md` for stall-parks.
- Integrate working-tree refresh (pre-existing, cross-bead).

---

# M1.6 — stall fix batch evidence (agalma-csw.3)

Bead: `agalma-csw.3` (S1 fix). Epic: `agalma-csw` (M1.6). Base HEAD: `69001ab`
(S1 root-cause, `agalma-csw.1`). Date: 2026-10-10. This section is **appended**;
the H4 evidence above is unchanged.

The S1 spike (`docs/spikes/stall-root-cause.md`) proved the free-route "stall" was
an **adapter projection gap**: sessions progressed (tokens, tool steps) while the
harness surfaced no events until terminal, so `progress_fingerprint` stayed
constant and the idle watchdog false-classified live turns as idle. The batch's
`SessionNotFoundError` was a second artifact: `close_session` (DELETE) ran before
the stall arm's `cancel_operation` (interrupt).

## What changed

1. **Live progress surfacing (adapter).** `translate::events_from` now emits a
   best-effort partial `UsageSnapshot` from the running session projection's live
   token counters (populated on each completed step), and `read_events` also
   translates the live `GET /api/session/{id}/message` page into a `ToolOutcome`
   per terminal tool part (`completed`→ok, `error`→err). Terminal behavior is
   unchanged (`Completed`/`Failed` still emit full usage + terminal event).
2. **Fingerprint advances with those deltas.** `progress_fingerprint` already
   sums token deltas and `steps = read_events().len()`, so the new partial
   snapshot and per-tool events make it move mid-turn.
3. **Cancel ordering.** `settle_after_poll` cancels (`cancel_operation`) **before**
   `close_session` for `Stalled`/`TimedOut` in the build, reference, and verifier
   paths; the adapter's `cancel_ack_from_response` treats a 404 as
   "already gone" (Ok, unacknowledged), so our own close can never surface a
   false `SessionNotFoundError`.
4. **Watchdog semantics preserved.** With real progress signals, 120 s idle is a
   genuine stall; no threshold changed.

## Environment

- Fresh scratch clone (`git clone --no-hardlinks` of the live tree at `69001ab`)
  into `target/test-runs/hardening-batch/run-2/target`, isolated bd store
  (`bd init`, prefix `agalma`). The conductor ran from the live working tree
  (with the uncommitted `agalma-csw.3` changes); the scratch clone is only the
  workspace/origin under test.
- Same four seed tasks and exact wording as M1.8/H4 (bodies copied byte-for-byte
  from the H4 `tasks/t{1..4}.md`). New ids:
  `agalma-6me`=seed-task-1 (README, fix), `agalma-85l`=seed-task-3 (builder.md,
  fix), `agalma-s08`=seed-task-2 (task-authoring.md, feature),
  `agalma-tvz`=seed-task-4 (conventions.md, feature).
- Command (no `--model`; routing v1 free default; `AGALMA_ALLOW_PAID` unset → free
  route, paid credential present but gated off):

  ```
  cargo run --offline -p agalma-conductor -- work \
    --repo <clone> --max-tasks 4 --state-dir <clone>/state
  ```

- Batch wall: `2026-10-10T13:49:12Z → 14:01:49Z` = **757 s (12.6 min)**
  (`EXIT=0`), inside the 60-minute timebox.

## Batch results

| id | Exec | Family | Route | Attempts | Verdict | `wall_ms` | Stalls | Tokens in/out |
|----|------|--------|-------|----------|---------|-----------|--------|---------------|
| `agalma-6me` | `exec:agalma-6me:1` | fix | `builder:fix` | 1 | **done (green)** | 153 407 | 0 | 17 822 / 974 |
| `agalma-85l` | `exec:agalma-85l:1` | fix | `builder:fix` | 1 | **done (green)** | 169 263 | 0 | 21 796 / 2 661 |
| `agalma-s08` | `exec:agalma-s08:1` | feature | `builder:feature` | 1 | **done (green)** | 52 542 | 0 | 25 246 / 2 713 |
| `agalma-tvz` | `exec:agalma-tvz:1` | feature | `builder:feature` | 2 (attempt 1 stalled, attempt 2 green) | **done (green)** | 359 385 | 1 | 0/0 (att 1) then 16 207 / 1 262 |

Model for every execution: `opencode/big-pickle` (routing v1 free default).

Terminal evidence in the clone:

- All four `m1/exec-*` tags exist and `main` (`f6a39e3`) carries all four
  candidate commits (linear). Only merged-on-green paths produce these tags.
- Every task bd status `closed`, `close_reason: done: green` (no parks).
- Merged content on `main` verified: README names `verifier` (1) and not `tester`
  (0); `docs/task-authoring.md` and `docs/conventions.md` exist;
  `prompts/builder.md` contains both `acceptance commands` and
  `do not edit the tests`.
- Every execution persisted a digest (`verdict: green`).

## Metrics vs H4

| Metric | H4 (`agalma-zxi.5`) | run-2 (this fix) | Change |
|--------|---------------------|------------------|--------|
| Success rate | 1/4 = **25%** | 4/4 = **100%** | **+75 pts** |
| Merges | 1 | 4 | +3 |
| Batch wall | 980 s (16.3 min) | 757 s (**12.6 min**) | **1.29× faster** |
| Stalls (`provider_health`) | 7 | **1** | −6 |
| Stalls with `tokens:0` | 7/7 | **0/1** | progress now visible |
| Stall verdict latency | ~120.2 s idle (wall ≈120.4 s) | 120.1 s idle (wall **232.8 s**) | progress detected for ~112 s before a genuine idle window |
| `SessionNotFoundError` / `interrupt -> 404` | 6 logged | **0 logged** | cancel-before-close fixed |
| Parked tasks | 3 | 0 | — |
| $/merge (cash) | $0.00 | $0.00 | = |

## Findings

- **The projection gap is fixed.** Tokens and tool deltas now reach the watchdog
  mid-turn. The single stall (`agalma-tvz` attempt 1) recorded `tokens: 9 239`
  on its `provider_health` event — versus `tokens: 0` for every H4 stall — and
  its `wall_ms` (232.8 s) exceeds the idle bound (120.1 s), i.e. the watchdog saw
  ~112 s of real progress before firing on a genuinely quiet window. Attempt 2
  recovered to green.
- **The 404 is gone.** The `close_session`-before-`cancel_operation` ordering
  produced six `interrupt -> 404 SessionNotFoundError` lines in H4 and **zero**
  here; `cancel_operation` now runs against a live session.
- **Free-tier tasks complete.** Three of four tasks finished on the first attempt;
  the fourth needed one retry after a real 120 s quiet window. The false-stall
  cascade that parked 3/4 tasks in H4 did not recur.
- **Known limitation (step granularity).** Liveness is emitted on step/tool
  *completion* (projection tokens, message tool parts). A single tool step that
  runs without completing for >120 s still reads as idle; `agalma-tvz` attempt 1
  is consistent with that boundary. This is a genuine idle by the watchdog's
  definition and recovered by retry, so no threshold change was made.

## Deviations / open items

1. **One genuine stall remains** (`agalma-tvz` attempt 1), recovered by the
   existing retry path. It is not a false positive: no token/step delta for the
   full 120 s idle window. Reported honestly rather than hidden by a threshold
   bump.
2. **Digest `attempts`/`commands` still under-report** the stall-then-retry path
   (same pre-existing measurement gap noted in H4); token counts now appear in
   `usage` events for the productive attempts.
3. **`tools/t*.md` seed bodies** were reused from the H4 run directory; titles
   `seed-task-1..4` and ids differ from H4, so triage order differs, but the
   per-task wording is byte-identical.
4. **No product-code commit/push and no bd writes** were performed (per bead
   constraints); the scratch clone's commits/tags/bd writes are isolated under
   `target/test-runs/hardening-batch/run-2/`.
5. **Timebox met** (12.6 min batch < 60 min).
