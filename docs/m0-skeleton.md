# M0 — skeleton design

Task: `agalma-4ui.1` (M0-D1). Epic: `agalma-4ui`. Status: design. Authored by
orchestrator; gates all implementation beads. Implementation via worker waves;
orchestrator reviews, commits, closes beads.

Refs: `docs/architecture.md` §2, §3.1, §3.6, §13 M0; `docs/component-contracts.md`
(Contract tiers, Harness contract); S0 results: `docs/spikes/s0a-harness.md`,
`s0b-confinement.md`, `s0c-execution.md`, `s0d-seam-freeze.md`;
`docs/m0-dependencies.md` (approved crates).

## Scope and exit

M0 = conductor daemon that runs ONE hardcoded task end-to-end against a fixture
repo, with durable state and recovery. Exit (epic acceptance):

- conductor restart recovers the task at its durable phase;
- crash after a Git update but before recording completion neither duplicates
  the update nor loses completion;
- surviving worker processes are reconciled (terminated, evidence recorded)
  before redispatch;
- ledger reflects the run; kill switch stops scheduling and survives restart.

Out of scope: bd queue, real tasks, bench, genome mutation, deciders, budgets
beyond `max_attempts`, dashboards, multi-target.

## Workspace layout (cargo workspace)

```
Cargo.toml               # workspace; [workspace.dependencies] with pinned versions
crates/agalma-contracts/ # types, IDs, traits, errors; no I/O, no vendor, no impl deps
crates/agalma-ledger/    # LedgerApi: SQLite via rusqlite; SQL stays private
crates/agalma-execution/ # ExecutionApi: serial executor, receipts, reconcile
crates/agalma-sandbox/   # SandboxApi: Seatbelt launch/terminate; profile render
crates/agalma-workspace/ # WorkspaceApi: git CLI; checkout/merge/reconcile
crates/agalma-harness-opencode/ # HarnessApi: OpenCode adapter (vendor confined)
crates/agalma-conductor/ # bin `agalma`: composition root, daemon, CLI
fixtures/target-template/ # plain files; copied + git-init at runtime
```

Dependency direction: conductor → impls → contracts only. No impl crate depends
on another impl crate. Enforced by an architecture test in
`agalma-conductor` (source scan; fails on cross-impl imports) plus workspace
membership.

## Contracts (frozen for M0)

IDs are opaque strings, typed newtypes; grammar:

- `TaskId`: fixture task id (`fix-answer`)
- `ExecutionId = exec:<task_id>:<generation>` (deliberate rerun = new generation)
- `OperationId = op:<execution_id>:<step>` (stable across delivery retries)
- `AttemptId = attempt:<n>`, `SessionId = session:<attempt_id>:<n>`
- `ArtifactRef = artifact:<name>`
- `BindingId`; `BindingRecord { binding_id, component_api, api_version,
  implementation, implementation_version, capabilities, required_capabilities,
  optional_capabilities, config_hash, generation, state }` (S0d folded)

Traits (M0 method sketch; M0.1 fixes signatures):

- `LedgerApi`: `commit(batch) -> CommitReceipt` (atomic state+event+receipt+
  intent), `execution(id)`, `pending_intents()`, `events(id)`, `kill_latch()`,
  `set_kill_latch(bool)`, `bindings()`, `schema_version()`.
- `ExecutionApi`: `start(task)`, `step(execution)` (one logical operation),
  `recover()` (boot: reconstruct → reconcile → dispatch), `cancel(execution)`,
  `status(execution)`.
- `SandboxApi`: `launch(LaunchSpec) -> SandboxChild`, `alive(&child)`,
  `terminate(&child) -> StopEvidence { process_group_gone, detail }`.
- `WorkspaceApi`: `prepare(template, exec) -> Checkout { path, base_sha }`,
  `diff(checkout)`, `integrate(checkout, expected_main_sha) -> MergeReceipt`,
  `tag(name, sha)`, `reconcile(checkout)`, `revert(sha)`.
- `HarnessApi`: S0d trait as folded in `component-contracts.md` (describe,
  start_attempt, create_session, run_turn, inspect_operation, read_events,
  cancel_operation, close_session, stop_attempt, `CancelAck`, `StopEvidence`).

Errors: `KnownFailure`, `UnsupportedCapability`, `Conflict`, `UnknownOutcome`
(thiserror). Normalized events: S0d set (`TurnStarted`, `ToolOutcome`,
`UsageSnapshot`, `TurnCompleted`, `TurnFailed`).

## Ledger schema v1

Tables (S0c seed + bindings): `meta`, `executions`, `operations`,
`execution_events` (monotonic sequence per execution), `dispatch_intents`
(`consumed` flag), `kill_latch`, `component_bindings`. Pragmas: WAL,
`synchronous=FULL`, `busy_timeout=5000`, foreign keys on. `schema_version=1`
recorded in `meta`; mismatched stored version parks, never auto-migrates.
No external I/O inside a transaction. Actual bundled SQLite version recorded in
`meta` at first open and reported by `status`.

## Hardcoded task (fixture)

`fixtures/target-template/`: tiny Rust crate; `answer()` returns 41; acceptance
test expects 42. Task descriptor `fixtures/target-template/task.yaml`:
`id: fix-answer`, `acceptance: [cargo test]`, `max_attempts: 2`,
`role: builder`. Conductor flags: `agalma run --fixture
fixtures/target-template [--once] [--state-dir <dir>] [--model <id>]`.

Phases: `intake → checkout → build → verify → integrate → done | parked`.

- checkout: `WorkspaceApi::prepare` copies template under
  `<state>/runs/<exec>/checkout`, `git init`, base commit, records base SHA.
- build: one harness session in the checkout under Seatbelt; prompt from
  `crates/agalma-conductor/prompts/builder.md`; model default
  `opencode/mimo-v2.6-flash-free` (env override).
- verify: Rust executes `cargo test` inside the sandbox (separate launch,
  checkout read-only outside target dir); exit code = verdict; output artifact.
- integrate: `WorkspaceApi::integrate` merges `candidate/<exec>` into `main`
  with expected old SHA; MergeReceipt { expected_main_sha, candidate_sha,
  result_sha }; ledger records receipt + tag `m0/<exec>`.
- retry: verify red → second build attempt with failure artifact referenced;
  attempts exhausted → `parked` + reason.

## Conductor daemon and kill switch

- One process; exclusive lock file in the state dir; default state dir
  `~/Library/Application Support/agalma` (`AGALMA_STATE_DIR` override).
- Dispatch loop is serial: one execution, one logical operation at a time.
- Latch: SIGTERM/SIGINT handler persists `kill_latch` via `LedgerApi` before
  exit; sentinel file `<state>/STOP` also checked before every dispatch.
  `agalma resume` (human) clears both. Latch active → no dispatch, status
  reports blocked.
- Boot: schema check → `ExecutionApi::recover` (reconstruct from events, no
  I/O; reconcile pending: workspace git state, worker pid liveness via
  SandboxApi evidence, intent delivery) → resume dispatch unless latched.

## Recovery matrix (M0.8 exit tests)

1. Crash mid-build: restart reconstructs phase; orphaned worker terminated
   with evidence before redispatch; no duplicate session/effect.
2. Crash after merge before completion record: restart detects `main` SHA ==
   MergeReceipt result SHA, completes ledger/tag without re-merging.
3. Duplicate delivery of a completed operation: recorded receipt returned, no
   effect.
4. Kill latch set → restart → no dispatch; `resume` clears; work continues.
5. Surviving worker with stale lease: terminated before redispatch.
6. Schema version mismatch: refuse to run; park execution.

Reuse S0c fixture cases where the semantics overlap (tests ported into
`agalma-ledger`/`agalma-execution`).

## Sandbox details

Product profile at `crates/agalma-sandbox/profiles/worker.sb` (from S0b, params
`ATTEMPT`, `PROTECTED`, `SOCKDIR`, `EXTRA_RO`, `EXTRA_RO2`, `EXTRA_RO3`). The
launcher always renders all three `EXTRA_RO*` slots; unused slots carry an inert
placeholder (`/dev/null`, already read-permitted) because `sandbox-exec` rejects
an empty `(subpath ...)` pattern. Conditions: fail-closed render
gate; attempt-scoped `TMPDIR`/`XDG_*`; toolchain read roots rendered;
termination signals descendants explicitly (macOS EPERM on group signal) then
verifies; `mach-lookup` stays broad for M0 with a TODO + open item.

## Harness adapter details

Spawn `opencode serve` under `SandboxApi` on a loopback port with
`OPENCODE_PASSWORD` generated per attempt; Basic auth; free model default;
reconciliation via session projection (durable log emitted only a sync marker —
S0d [open]); cancel = interrupt then confirm; stop = process evidence. Vendor
types/HTTP/auth/XDG confined to the adapter crate. User OpenCode state never
touched; isolated XDG under the attempt dir.

## Test strategy and verification commands

- Unit tests co-located in each crate (`cargo test -p <crate>`).
- Integration tests in `agalma-conductor/tests/`; run dirs under
  `target/test-runs/` (never /tmp); state dirs per test via `AGALMA_STATE_DIR`.
- Per-bead commands: `cargo build --workspace --offline`,
  `cargo test --workspace`, `cargo clippy --workspace -- -D warnings`,
  `cargo fmt --check --all`. Orchestrator runs them before every bead commit.
- M0.7 smoke: `cargo run -p agalma-conductor -- run --fixture
  fixtures/target-template --once --state-dir target/test-state` exits 0;
  `git log` in the run checkout shows the merge; ledger reflects done.
- M0.8: scripted crash/kill/restart cases (matrix above) as integration tests.

## Worker wave plan

- B: M0.1 `agalma-4ui.3` (single; freezes contracts crate).
- C: M0.2 `4ui.4`, M0.4 `4ui.6`, M0.5 `4ui.7` (parallel; disjoint crates).
- D: M0.3 `4ui.5`, M0.6 `4ui.8` (parallel).
- E: M0.7 `4ui.9` (single).
- F: M0.8 `4ui.10` (single).
Workers: `slugineer-worker`; no commits, no bd writes; orchestrator commits per
bead and closes.

## Open items inherited

- Durable session log persistence (S0a/S0d) — adapter reconciles via projection.
- macOS group-signal EPERM (S0b/S0d) — explicit descendant termination.
- `/api/provider` not authoritative (S0a) — bind by configured model, verify at
  first turn.
- `mach-lookup` narrowing (S0b) — TODO in profile, open for M3 hardening.
- `OperationState` `Cancelled` variant (S0d) — M0 uses terminal
  `Failed { reason: "interrupted" }`; product freeze question.
