# M1 — working loop design

Task: `agalma-52k.1` (M1-D1). Epic: `agalma-52k` (M1). Status: design. Authored by
orchestrator; gates all M1 implementation beads. Implementation via worker
waves; orchestrator reviews, commits, closes.

Refs: `docs/architecture.md` §4 (loop), §5 (task model), §6 (sessions), §7
(evaluation), §13 M1; `docs/component-contracts.md`; `docs/m0-skeleton.md`
(implemented M0 surface); `docs/m0-exit-evidence.md`.

## Scope and exit

M1 turns the M0 single-task conductor into a queue-driven loop over real repo
tasks. Exit (architecture §13 M1):

- seeded batch processed; success rate and $/merge measured;
- bd intake with stable execution IDs, fenced claims, reconciled status
  projections;
- builder/verifier phases, independent checkouts, separate sandboxed
  verification, deterministic acceptance, merge on green, cost recorded,
  digest generated;
- routing/triage through the versioned decision contract with static policies
  and recorded outcomes;
- duplicate dispatch and expired-lease recovery cannot merge twice;
- staged harness rebind between attempts preserves the conductor scenario and
  keeps recovery on the pinned implementation.

Decisions fixed here:

- **Target**: agalma repo; seeded batch is data/docs scope (prompts, docs,
  fixtures) per the M1 mutation ladder (§8). No pipeline Rust in tasks.
- **Cost basis**: free model via the M0 egress proxy; `$0.00` merges are
  expected and valid; provider-auth proxy and paid models remain open items
  (needed before M3 budgets).
- **Verifier**: agent-side diagnosis only; Rust runs acceptance commands.
  Fresh session per (task, phase, attempt); handoff via files.

Out of scope: budgets enforcement, scheduler/cadence, bench/promotion, code
self-modification, GitHub intake, parallelism, paid credentials.

## Task model and bd adapter

Crate `agalma-taskqueue`; `TaskQueueApi` trait added to `agalma-contracts`.
bd CLI (`std::process::Command`) is private to the crate. Machine interface:
`bd list --json`, `bd show <id> --json --include-comments`,
`bd update <id> --claim` (atomic), `bd comment <id>`, `bd close <id>`,
`bd update --add-label`.

Task = bd issue + fenced YAML block in the description:

```yaml
directive: directive/v0
target: agalma
ref: main
acceptance:
  - sh -c '...'
budget_usd: 2.00
priority: normal
family: fix
max_attempts: 2
```

Hand-rolled strict subset parser (scalars + string lists); missing/invalid
block → issue is not a task (skipped, counted in reconcile report).

Identity: `TaskId` = bd issue id; `ExecutionId = exec:<task_id>:<task_generation>`;
`task_generation` increments when a claim cycle starts fresh (reconcile keeps
the existing generation). `lease_generation` is a ledger counter fenced
against `OperationId`s and worker records.

Claim: `bd --claim` first (atomic), then ledger commit records claim + lease
before dispatch; interrupted starts reconcile by identity (§3.6). Local bd
locking makes multi-conductor claims safe for M1 (single conductor anyway).

Projection: on every phase transition the adapter appends a
`bd comment` (`phase=… execution=… lease=…`); terminal: `done` → `bd close`,
`parked` → label `parked` + comment with reason. Ledger is source of truth
for execution progress; reconcile compares projection vs ledger and rewrites
the projection (never the ledger).

## Decision contract

Crate `agalma-decision`; `DecisionApi` + request/response types in contracts.

- Versioned `DecisionRequest`: stable operation ID, decision kind, bounded
  context/artifact refs, eligible option IDs, deadline, pinned policy versions.
- M1 decision kinds: `triage.pick-next` (eligible ready tasks → pick via
  static policy: priority, then age, then id) and `retry.escalate`
  (attempt/verdict based mechanical choice). Static baselines first; no model
  inference required for M1 exit.
- Durability: request persisted (ledger, schema v2 `decisions`) before apply;
  validated response or fallback recorded with receipt; recovery reuses the
  recorded outcome; changed inputs → new operation ID.
- Outcomes: append-only decisions table + projection query for the digest.

## Ledger schema v2

Additive migration from v1: `decisions`, `digests` tables; `SCHEMA_VERSION`
becomes 2. `open()` migrates v1 → v2 only when all executions are terminal;
otherwise refuses (no in-flight migration). WAL/FULL pragmas unchanged. Cost
and usage are derived from existing `execution_events` (UsageSnapshot) — no
new columns; digests store derived summary rows plus artifact paths.

## Loop (conductor)

`agalma work --repo <path> [--once] [--max-tasks N] [--state-dir] [--model]`:

1. intake: queue adapter lists ready tasks; decision `triage.pick-next`
   (static policy) picks one; claim + execution identity recorded.
2. checkout: workspace **repo mode** — `git clone --no-hardlinks <repo>
   <run>/checkout` at task `ref`, candidate branch `candidate/<exec>`
   (own Git metadata; shared object store avoided).
3. build: builder session (existing harness path) in the checkout; prompt
   from `crates/agalma-conductor/prompts/builder.md`, now parameterized with
   task acceptance commands and prior diagnosis.
4. verify: fresh verification checkout at the candidate SHA (pristine),
   acceptance commands run by Rust, each as its own sandboxed launch;
   failures captured. Red → verifier diagnosis session (fresh, read-only diff
   + failure output) produces `artifacts/diagnosis@<n>.md`; builder retry
   uses it; bounded by `max_attempts`; exhausted → postmortem note + parked.
5. integrate: workspace merges candidate into origin `main` with expected-SHA
   CAS (`git -C <origin> update-ref refs/heads/main <new> <old>`), tag
   `m1/<exec>`; MergeReceipt recorded. Green only.
6. record + digest: cost/usage summary, files touched, commands+outcomes,
   repairs, wall time; digest row + JSON artifact; bd projection updated.

Keep: SIGTERM latch, STOP sentinel, boot recovery ordering, survivor
termination, `--once` for tests.

## Fencing and recovery (cannot merge twice)

- Claim → ledger records lease_generation; dispatch intents carry it.
- Duplicate dispatch: operation receipt returns recorded result; integration
  effect probe = origin main already at candidate SHA.
- Expired lease: recovery reconciles first (workspace git state, worker
  liveness, bd claim); merge already landed → record receipt, complete; no
  re-merge. Ambiguous → park.
- Crash between CAS merge and completion: boot detects `origin main ==
  candidate_sha` (existing M0.8 probe generalizes) and completes.
- bd projection reconcile on boot (comments/status vs ledger), ledger wins.

## Harness rebind drill

New crate `agalma-harness-reference`: deterministic `HarnessApi` adapter
(scripted turns from files; no vendor, no network). Binding selection:
`AGALMA_HARNESS=opencode|reference` resolved at attempt start; binding record
persisted per operation (S0d `BindingRecord`). Test: attempt 1 red via
reference, config flipped, attempt 2 via opencode (or vice versa) completes;
crash between attempts recovers on the **pinned** binding for in-flight
operations. Recovery stays on the pinned implementation; new attempts may
rebind between operations.

## Seed batch (data/docs scope)

Four real bd tasks created at M1.8 start (exact wording then), all against
agalma with `acceptance` shell one-liners run by Rust:

1. README drift fix (roles/loop) — grep-based checks.
2. `docs/task-authoring.md` describing the fenced block — file/content checks.
3. `crates/agalma-conductor/prompts/builder.md` improvement (acceptance
   commands + no test edits) — content checks.
4. `docs/conventions.md` (or similar) phase-handoff rules — content checks.

Sequence rule: seed tasks must not touch pipeline Rust or `Cargo.toml`
(`sh -c 'git diff --name-only <base>..<cand> | grep -v "\.md$|prompts/"'`
style guard expressed as acceptance). The running conductor reads its prompts
per dispatch, so prompt tasks take effect next attempt (intended M1 data
self-improvement).

## Test strategy and verification commands

- Unit tests co-located per crate; integration in `agalma-conductor/tests/`;
  run dirs `target/test-runs/*` (never /tmp); state dirs via `AGALMA_STATE_DIR`.
- Deterministic tests use the reference adapter + scripted activity mode; one
  live e2e per wave where useful (free model, ~3 min).
- Per-bead: `cargo build --workspace --offline`, `cargo test --workspace
  --offline`, `cargo clippy --workspace --all-targets --offline -- -D
  warnings`, `cargo fmt --check --all`.
- M1.8 exit run: seeded batch, then `docs/m1-exit-evidence.md` with success
  rate, $/merge, wall time, fencing/rebind evidence.

## Crate map delta

- new: `agalma-taskqueue`, `agalma-decision`, `agalma-harness-reference`.
- changed: contracts (TaskQueueApi, DecisionApi), workspace (repo mode +
  origin CAS integrate), ledger (schema v2), conductor (work loop, digest,
  projection), harness-opencode (prompt parameterization only).

## Worker waves

- B: M1.1 `agalma-52k.2` (queue), M1.5 `agalma-52k.6` (decision+ledger v2) —
  parallel.
- C: M1.2 `agalma-52k.3` (loop + workspace repo mode) — single.
- D: M1.3 `agalma-52k.4` (verifier), M1.4 `agalma-52k.5` (acceptance+digest),
  M1.6 `agalma-52k.7` (fencing) — parallel.
- E: M1.7 `agalma-52k.8` (rebind) — single.
- F: M1.8 `agalma-52k.9` (seeded batch + evidence) — single.

## Open items carried

- Provider-auth proxy / paid models (before M3 budgets).
- `OPENCODE_CONFIG_DIR` discovery workaround; mach-lookup narrowing.
- `OperationState::Cancelled` freeze question.
- Durable session log persistence.
- Workspace CAS integrate assumes local origin path (single-host MVP).
