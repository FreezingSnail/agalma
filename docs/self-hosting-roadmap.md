# Self-hosting roadmap — "strapping itself up"

Status: orchestrator draft, 2026-10-12. Scope: the path from *agalma works its own
data/docs backlog with human gates* to *agalma decomposes, builds, verifies, and
promotes its own work unattended* (M2 → M3 → P1), with the near-term enablers that
unblock each stage.

Refs: `docs/architecture.md` §13 (milestones), `docs/ipc-exit-evidence.md`,
`docs/b1-f3-evidence.md`, `docs/tui-design.md`, `docs/planning-pipeline.md`,
`docs/paid-route-evidence.md`.

## Where the factory stands (2026-10-12)

| Capability | State | Evidence |
|---|---|---|
| Conductor loop, ledger, kernel, crash recovery | ✅ M0/B0 | `docs/m0-exit-evidence.md`, `docs/b0-exit-evidence.md` |
| Queue-driven loop over real repos, merge-on-green | ✅ M1 | `docs/m1-exit-evidence.md` (2/4 merged, $0.00 free tier) |
| Tier-1 seams behind replaceable boundaries, dual-binding parity + rebind/rollback | ✅ B1, now all on ConnectRPC over UDS | `docs/b1-exit-evidence.md`, `docs/ipc-exit-evidence.md` |
| Live paid-route process smoke (OpenCode harness) | ✅ | `docs/b1-f3-evidence.md` (1 attempt, $0.0058) |
| Pipeline: goal → concierge → epic + design → staged decomposition → approval → fleet | ✅ implemented; pilot pending | CP.1–CP.5 landed; `agalma-i5h.7` pilot open |
| Review gate (post-merge drift vs epic design, blocking drift-fix) | ✅ | `agalma-i5h.6`, `docs/planning-pipeline.md` |
| TUI control plane (read model, watch, components, economics, controls, ratatui) | ✅ | `agalma-3kr` epic; ledger v4 control receipts |
| Bench + promotion + genome/memory | ❌ | `agalma-cg9` (M2 epic, no children) |
| Scheduler, budgets enforcement, always-on | ❌ | `agalma-3q5` (M3, blocked by M2) |
| Autonomy gate (no human review) | ❌ | `agalma-52v` (P1, blocked by M3) |

Key constraint today: the loop builds tasks whose acceptance runs offline, and the
M1 mutation ladder keeps tasks in data/docs scope. Rust/pipeline self-modification
stays behind transitional human oversight **until the promotion + autonomy gates
exist** (architecture §13: "Code stays under transitional oversight until P1").

## Gap analysis — what "strapping up" requires

1. **Pipeline pilot never exercised end to end** (`i5h.7`): no proof yet that a goal
   travels intake → design → decomposition → approval → execution → review → close.
2. **Transport defaults still in-process** (harness/ledger selectors `_ => InProcess`).
   Live smoke passed with `=process`; the flip was deferred on a real blocker
   (`SUN_LEN` on `<state>/harness-ref-N.sock` for reference runs). Connect is the wire
   default, so the flip is now a path-length fix + re-smoke, not new machinery.
3. **Flaky verification undermines unattended trust**: `agalma-qww` (harness_connect
   `Connection refused` under parallel load) is one instance of a class — readiness
   races in spawn/connect paths must be deterministic before always-on.
4. **No promotion machinery**: bench fixtures/history replay, paired candidate-vs-
   baseline evaluation, immutable candidate SHAs, regression veto, auto-rollback
   tagging, genome/prompt versioning (M2, `agalma-cg9`).
5. **No scheduler/cadence/budget enforcement**: per-task `budget_usd` is recorded but
   not enforced; no unattended 72h operation, supervision, backups, retention (M3).
6. **Human review is still the gate**: decomposition approval (`undefer`) and code
   oversight are manual until the P1 gate proves the required list.

## Staged roadmap

### R0 — close the self-referential loop (now; days)

Goal: agalma processes **its own planning backlog** with one human touch (goal spec +
undefer) and zero per-task handholding.

- `agalma-i5h.7` pilot: run concierge/designer over the TUI epic, approve, let the
  fleet consume staged children (proves the full CP path).
- `agalma-qww` + readiness-race sweep: make spawn/connect waits deterministic
  (poll-connect or READY-line gating) so parallel/unattended runs don't flake.
- Transport flip package: bound the per-run socket paths (fix `SUN_LEN`), flip
  `HarnessTransport` default to process, re-run the live smoke on Connect, then
  evaluate peer boundaries for the same flip (record deferrals where unsafe).
- Expose the pipeline loop as a conductor command cadence (concierge sweep already
  exists; wire goal list → staging → dispatch as one `agalma` flow).

Exit: one goal spec → staged beads → fleet executes → review gate runs → epic closed,
with the ledger + digests showing every step.

### R1 — factory runs its own data/docs backlog unattended (M2 part 1)

- Directive ingestion (goal list file) + staged approval policy: auto-`undefer`
  for classes the policy marks safe (docs, planning, prompts) with review gate.
- Budget enforcement minimal: hard-fail a task when recorded spend crosses
  `budget_usd` (needed before unattended code work).
- Postmortem mining: turn parked executions into new candidate tasks
  (`docs/planning-pipeline.md` CP.5 review flow).

Exit: a batch of agalma's own data/docs beads completes with the human only seeding
goals; failure classes land in postmortems instead of dropping work.

### R2 — self-modification behind promotion gates (M2 core; `agalma-cg9`)

Decompose M2 into children modeled on the B1 recipe (each with acceptance + evidence):

- **Bench**: fixtures + history replay; paired candidate vs baseline (K trials,
  statistics per architecture §6.6); anti-gaming (pristine ref, read-only tests).
- **Promotion pipeline**: candidate → deterministic acceptance → independent
  evaluation → paired bench → promote (commit + tag) | reject/revert (memory);
  immutable candidate SHAs; regression veto; auto-rollback of bad promotions.
- **Mutation ladder**: data/docs (done) → prompts/skills (genome versioning,
  auto-promotion of wins) → pipeline Rust behind the promotion gate.
- **Nerve core**: repair, context injection, permission bridge, compaction
  summarizer (OpenCode TS plugin; spike: `spikes/s0a/probe-plugin.js`).
- **Model onboarding**: screen + full eval + routing mutation under policy;
  decider candidates in shadow mode before activation.

Exit (M2): factory improves its own prompt/skill unattended, keeps the win, and the
ledger shows it — under transitional oversight for code, automatic for genome.

### R3 — always-on baseline (M3; `agalma-3q5`)

- Scheduler + cadence; recon/mining; digest cadence; degradation alerts (TUI
  economics/alerts already surface the data).
- Budget enforcement full: failed, local, and remote inference.
- Conductor supervision (launchd/service), SQLite backups/restore, retention,
  schema-upgrade procedure; recovery drills.

Exit (M3, MVP): 72h unattended; budget respected; clean kill/resume; alerts fire;
restore drill reconciles execution state, receipts, bd, and Git before dispatch.

### R4 — autonomy gate (P1; `agalma-52v`)

Prove the §13 list — protected acceptance/evaluator fixtures, reproducible
verification against immutable SHAs, paired bench + regression veto, directive and
budget enforcement, crash/lease/cancellation reconciliation, automatic rollback,
directive-bounded actions — via shadow + canary, then enable autonomous code/kernel
integration under the active directive. Regressions disable promotion and revert
without human review.

## Immediate bead plan

| Action | Bead | State |
|---|---|---|
| Pipeline pilot | `agalma-i5h.7` | ready (live, needs undefer) |
| Flake fix (readiness race) | `agalma-qww` | ready |
| Socket-path bound + transport default flip + re-smoke | *to file* (R0) | new |
| M2 decomposition (bench/promotion/nerve/model onboarding) | `agalma-cg9` children | *to file* (design first) |
| M3 / P1 | `agalma-3q5`, `agalma-52v` | epics exist, blocked |

Sequencing law: **verification determinism → transport parity → promotion gates →
always-on → autonomy**. Skipping ahead (unattended code edits without bench/promotion/
rollback) is the failure mode this roadmap exists to prevent.
