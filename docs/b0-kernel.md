# B0 — durable execution kernel design

Bead: `agalma-9n4.1` (B0-D1). Epic: `agalma-9n4`. Status: design.
Orchestrator-authored; gates all B0 beads. Ref:
`docs/durable-execution-bootstrap.md` (requirements), `docs/architecture.md` §3
(execution/ledger/decisions), `docs/component-contracts.md` (Tier-1 seams),
M0/M1 implementation (`crates/agalma-execution`, `agalma-ledger`).

## Scope and exit

Develop a durable execution kernel behind the frozen `ExecutionApi`; the M1
seed executor (`agalma-execution`) keeps running the factory until a candidate
passes the gates and is activated at a controlled restart. Candidates never
touch the live ledger or replace the running factory during evaluation.

Exit (bootstrap doc): gate-passing kernel resumes interrupted tasks and waits,
rejects stale workers, reconciles crash-after-merge, preserves pending state
across a compatible upgrade, meets measured local resource targets; consumer
API unchanged; compatible state export/import.

## Placement and layering

- New crate `agalma-kernel` implementing `ExecutionApi`; persistence only
  through `LedgerApi`; activities registered through component contracts
  (harness/sandbox/workspace/decision), never called from the transition
  function.
- `agalma-execution` (seed) remains; both satisfy the same trait. The conductor
  selects the binding (S0d binding records + staged rebind), so activation and
  rollback are config flips with pinned recovery.
- Kernel does not own policy, sandbox lifecycle, or integration; it owns
  progression, retries, deadlines, directive updates, cancellation, recovery.

## Execution model and history

- Versioned `ExecDef` (`exec_def/v1`): named states and commands; M1 phase
  machine generalized to `Pending | Ready | Running | Waiting | Completed |
  Failed | Parked | Cancelled` per execution plus per-operation lifecycle.
- Commands: `Start`, `Deliver(op)`, `Decision(decider)`, `TimerFired(id)`,
  `DirectiveUpdate(v)`, `Cancel`, `KillLatch`, `Retry`.
- Pure transition: `transition(state, command, recorded inputs) -> Vec<Effect>`
  where effects are `Commit(batch)` / `Dispatch(intent)`; no I/O, no model
  calls, no tools. Deterministic replay from history.
- History: append-only `execution_events` (monotonic sequence per execution);
  current-state projection updated in the same atomic `LedgerApi` commit;
  journal and projection must agree (reducer check on load).
- Large artifacts stay outside history as immutable references (artifact refs,
  diff digests, outputs); history stores references only.
- Unknown `exec_def` version or ambiguous external effect → parked, never
  guessed.

## Ledger schema v3 (additive over v2)

New tables: `timers(timer_id PK, execution_id, kind, fire_at, state, payload)`,
`directives(directive_id PK, version, source, state, payload, received_at)`,
`kernel_meta(key, value)` for `exec_def_version`, `kernel_version`,
`resource_baseline`. Migration v2→v3 follows the existing rule: only when all
executions are terminal; otherwise refuse (no in-flight migration). Export/
import: `LedgerApi::export_state` / `import_state` producing a canonical JSON
snapshot (history + projection + timers + directives + bindings) with a version
header; import refuses incompatible versions.

## Activity protocol

- `ActivityDef { name, version }` registered in a kernel registry; handlers
  expose `execute(ctx) -> Outcome` and `reconcile(ctx) -> EffectProbe`.
- Stable operation IDs (`op:<execution>:<step>[@attempt]`) + input hashes;
  reusing an ID with different contents is rejected (Conflict).
- Results are persisted before advancing; duplicate delivery returns the stored
  result. Pending operations reconcile external state before retry.
- Model attempts are explicit budgeted decisions (`DecisionApi`): request
  persisted before dispatch; validated choice/fallback recorded before
  dependent work; recovery reuses the recorded outcome without re-inference;
  changed inputs require a new operation ID.
- Operations pin component/API binding versions; retries reconcile with the
  pinned binding (no silent switch to the configured adapter).

## Durable waiting

- Retry schedules and deadlines are rows in `timers`; overdue timers fire once
  after restart via a recovered `TimerFired` command (idempotent by timer id).
- Directive updates: unique `directive_id`, bound to source + version;
  duplicates harmless; a newer directive can cancel or replace queued work
  (transition derives queued commands from the active directive).
- Waiting releases agent sessions and workers (no busy-wait; conductor sleeps
  until the next timer/dispatch).

## Cancellation and ownership

- Cancellation requests are persisted commands; lease generations fence stale
  workers (M1 machinery retained); reclaiming requires reconciliation of prior
  processes (evidence recorded, `SandboxApi` termination).
- Kill latch blocks recovered dispatch as well as new work; latch survives
  restart; `resume` clears.

## Upgrade, activation, rollback

- Persisted-contract versioning: `kernel_meta`; candidates are checked against
  saved fixture databases and histories (reducer-only reconstruction, no
  external effects); explicit migrations only.
- Staged activation: set latch → settle/reconcile in-flight executions → swap
  `ExecutionApi` binding at controlled restart (rebind rules from S0d) → schema
  compatibility check → clear latch → resume. Rollback: keep the seed binding
  and its recovery path; restore procedure reconciles ledger, bd, Git, and
  processes before dispatch.
- Resource gates: measure seed baseline first (idle RSS/CPU, restart time,
  dispatch latency, DB growth) and set numeric regression limits in K5;
  correctness is a veto.

## Protected recovery fixture pack

`fixtures/recovery-pack/` with a `MANIFEST.sha256`; an integrity test
(`crates/agalma-kernel/tests/pack_integrity.rs`) fails if any fixture changed —
candidates cannot edit the tests that judge them. Cases (fake effects /
disposable repos, crash injection before/after persistence and effect
boundaries): transition-commit-before-dispatch; duplicate delivery after
completion; restart after decision/fallback commit; merge-before-completion;
stale-lease worker; restart during retry delay/directive update; upgrade with
pending executions; kernel/adapter replacement; rollback/restore reconcile.
The pack runner emits a per-case gate report consumed by K5.

## Beads and waves

- D1 `9n4.1` design (this doc) — closes before implementation.
- B0.1 `9n4.2` fixture pack + crash harness (protected).
- B0.2 `9n4.3` kernel model/history/transition.
- B0.3 `9n4.4` activity protocol + decisions.
- B0.4 `9n4.5` durable waiting.
- B0.5 `9n4.6` cancellation/ownership.
- B0.6 `9n4.7` upgrade/activation/evidence.
Waves: B0.1 single → B0.2 single → B0.3 → B0.4 → B0.5 → B0.6 (kernel crate is
one evolving module; serial to keep history/transition stable). Each bead:
verify → commit → close by orchestrator.

## Constraints

No new crates without approval; offline builds after first fetch; workers do
not commit; fixtures/protected pack edits are gate failures; candidates run on
isolated state under `target/test-runs/` (never /tmp).
