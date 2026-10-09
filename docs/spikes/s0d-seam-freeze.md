# S0d — seam freeze: harness contract, binding exercise, decision baselines

Task: `agalma-zf5` (S0d). Status: spec ready. Timeboxed spike; kill/fallback
decision at exit. Owner of this spec: orchestrator. Implementer: worker.

Architecture refs: `docs/component-contracts.md` (Contract tiers, Harness
contract), `docs/architecture.md` §3.2/§3.7/§6/§13 S0d. Inputs: S0a harness
results (`docs/spikes/s0a-harness.md`), S0b confinement results
(`docs/spikes/s0b-confinement.md`), S0c execution results
(`docs/spikes/s0c-execution.md`).

## Purpose

Freeze the harness seam enough that the same conductor scenario runs under two
implementations — OpenCode and a deterministic reference adapter — by changing
only a binding selection. Prove API independence, record the static decision
baselines, and confirm the tier/ownership freeze. No product wiring.

## Exit criteria

- [ ] Same scenario passes under `config.reference.json` and `config.opencode.json`; the only difference is the config file.
- [ ] Normalized event sequences are structurally comparable (same event kinds, baseline required kinds present in both).
- [ ] Reference run is deterministic: byte-identical transcript across two runs.
- [ ] Capability fallback demonstrated: scenario requests an optional capability the reference adapter does not advertise; driver picks the declared supported plan instead of failing.
- [ ] Cancellation: operation reaches terminal state; OpenCode process tree is gone (no orphans) after `StopAttempt`.
- [ ] Privacy check: scenario/contract code contains no OpenCode endpoints, HTTP, SSE, or vendor names (grep check script exits 0).
- [ ] Static decision baselines for every decision kind in §3.7 are recorded below and exercised by a static evaluator smoke test.
- [ ] Ownership/ID/capability freeze recorded below; deltas against `component-contracts.md` listed.
- [ ] Verdict recorded; deviations and new open items listed.

## Deliverables

```
spikes/s0d/
  Cargo.toml, Cargo.lock        # std-only, zero dependencies, offline build
  src/contract.rs               # frozen types (below), no implementation deps
  src/reference.rs              # deterministic reference adapter
  src/opencode.rs               # OpenCode adapter (vendor details live ONLY here)
  src/scenario.rs               # scenario driver; imports contract only
  src/main.rs                   # arg parsing, binding selection, transcript output
  src/decisions.rs              # static baselines + smoke test
  config.reference.json
  config.opencode.json
  run_all.sh                    # builds offline, runs both bindings + checks
  privacy-check.sh              # greps scenario/contract for vendor leakage
  runs/                         # gitignored evidence (gitignore already covers)
docs/spikes/s0d-seam-freeze.md  # this file: fill Results
```

Constraints: zero downloads (std only; if HTTP needs more than `std::net` +
hand-rolled parsing, shell out to installed `curl` via `Command` — record which
approach). No git commit/push. bd read-only. Artifacts only under `spikes/s0d/`.
Never `/tmp`. Timebox: two failures per item → record blocker, continue.

## Frozen contract (spike scope, Rust)

`contract.rs` types; consumers use only these:

- `Describe { api: "HarnessApi", api_version: 0, impl_name, impl_version, capabilities: BTreeSet<String> }`
- Capability strings: `session_fork`, `synthetic_feedback`, `model_switch`,
  `constrained_generation`, `compaction`, `context_hooks`.
- IDs (canonical, opaque to scenario): `attempt:<n>`, `session:<attempt>:<n>`,
  `op:<attempt>:turn/<n>`, artifact refs `artifact:<name>`.
- `StartAttemptRequest { workspace, role, genome_ref, constraints_ref, limits, operation_id }`
- `CreateSessionRequest { role, model, prompts_ref, handoff_refs, tool_policy_ref }`
- `RunTurnRequest { input_ref, bounded_turns, deadline_ms }`
- `OperationState { Running, Completed { result_ref, usage }, Failed { reason }, Unknown }`
- Normalized events: `TurnStarted`, `ToolOutcome { tool, outcome: Ok|Error, detail }`,
  `UsageSnapshot { tokens_in, tokens_out, cost_usd, completeness: Complete|Partial|Unknown }`,
  `TurnCompleted { result_ref }`, `TurnFailed { reason }`.
- Errors: `KnownFailure`, `UnsupportedCapability`, `Conflict`, `UnknownOutcome`.
  Retryable error never authorizes repeating an effect (documented, not
  exercised here).
- Trait `HarnessAdapter`: `describe`, `start_attempt`, `create_session`,
  `run_turn`, `inspect_operation`, `read_events`, `cancel_operation`,
  `close_session`, `stop_attempt`.

Binding record (config): `{ binding_id, component_api, api_version,
implementation, implementation_version, capabilities, config_hash }`. Driver
verifies declared API/capability needs against `Describe` before running;
missing required capability → refuse to bind. Binding flips are config-only.

## Scenario (single driver, both bindings)

Steps: start attempt (workspace = fixture dir under `spikes/s0d/runs/<UTC>/`);
create session; run turn; read events to terminal; assert baseline kinds present;
assert usage snapshot completeness is declared (not silently zero); then a
second turn that is cancelled mid-flight (reference: cancel between scripted
events; OpenCode: interrupt); assert terminal state and cancellation
acknowledgment distinct from termination; `CloseSession`; `StopAttempt`;
assert cleanup evidence (OpenCode: process group gone; reference: cleanup log).

Capability sub-scenario: request optional capability `synthetic_feedback`;
driver consults `Describe`; absent → choose declared fallback (fresh session +
handoff note), log `fallback:<capability>`; required capability set stays empty.

OpenCode adapter (vendor details confined here): spawn
`opencode serve --hostname 127.0.0.1 --port 39003` with isolated
`XDG_CONFIG_HOME`/`XDG_DATA_HOME` under the run dir, `TMPDIR` inside it,
`OPENCODE_PASSWORD` set; Basic auth user `opencode`; prompt pinned free model
(default `opencode/mimo-v2.6-flash-free`, overridable by env); translate SSE
best-effort but reconcile via `GET /api/experimental/session/{id}/log?follow=true`
(S0a: SSE volatile); cancel via interrupt; `StopAttempt` = SIGTERM process
group then verify no descendant survives (S0b condition 3); user OpenCode state
must remain untouched. Never write `~/.config/opencode` or
`~/.local/share/opencode`.

Reference adapter: deterministic scripted event sequence (fixed events, fixed
usage values); cancel honors mid-sequence stop; cleanup log; advertises no
optional capabilities; two runs → byte-identical transcripts.

## Static decision baselines (`static/v1`)

Recorded here; smoke test proves the static evaluator answers each kind without
inference:

| Decision kind | Static baseline (`static/v1`) |
|---|---|
| Task classification / model routing | fixed map: role→model (`builder`→pinned free model, escalation ladder `[free, cheap, standard]` on attempts ≥ 2); no model call |
| Failure triage / next action | mechanical ladder: attempts < max → retry same route; verification red → repair attempt; error streak ≥ N → escalate; else park; no model call |
| Memory / context selection | priority order: mandatory instructions, task spec, prior-phase artifacts, top-K FTS; no scoring model |
| Task ranking | priority class, then FIFO; dependencies/claims filtered before ranking |
| Advisory tool checks | disabled at MVP |

Fallback semantics: timeout/unavailable/malformed/abstention → baseline,
recheck eligibility; late response never supersedes an applied fallback;
changed input → new operation ID. No eligible options → wait or park.

Decider adapter requirements (recorded from S0a): generative inference uses
`HarnessApi` one-shot constrained generation; specialized adapter runs as a
supervised process with read-only weights, no ledger/Git/tool authority, idle
unload; local resource measurement required before adoption. S0a caveats:
isolated model catalog showed only free provider models; `/api/provider` is not
authoritative for auth; `OPENCODE_API_KEY` integration did not populate it.

## Ownership / tier freeze (confirm against component-contracts.md)

Tier 1 seams: `HarnessApi`, `SandboxApi`, `WorkspaceApi`, `ExecutionApi`,
`LedgerApi`, `TaskQueueApi`. Capability profile `mvp-baseline`: required =
`CreateSession`/`RunTurn`/`InspectOperation`/`ReadEvents`/`CancelOperation`/
`CloseSession`/`StopAttempt` baselines; optional = the six capability strings.
OpenCode adapter may advertise optional capabilities only after S0-scope proof;
the harness binding exercise requires only `mvp-baseline`. Any deltas vs
`component-contracts.md` go in Results, not into that doc (orchestrator folds
them).

## Results

Run: `spikes/s0d/runs/20261009T180133Z/` (gitignored). Defining run:
`run_all.sh` exit 0; per-run evidence in `summary.txt`, `ref1/transcript.txt`,
`ref2/transcript.txt`, `ref.diff` (0 bytes), `oc/transcript.txt`,
`privacy.txt`, `decisions-smoke.txt`, `cargo-test.log`, `build.err`.

Environment: opencode **v2.0.15**; rustc/cargo 1.97.1; macOS 26.6.2 (arm64);
isolated `XDG_{CONFIG,DATA,CACHE,STATE}_HOME` + attempt-scoped `TMPDIR` under the
run dir; server `http://127.0.0.1:39003`; model
`opencode/mimo-v2.6-flash-free`. Zero crates: `cargo build --offline` and
`cargo test --offline` succeed; `Cargo.lock` pins only `s0d`.

### Exit criteria

| Item | Status | Evidence |
|---|---|---|
| both bindings, config-only flip | ✅ | same binary + same flags; only `--config` differs: `--config config.reference.json` vs `config.opencode.json`; both transcripts `result: PASS`. |
| comparable event kinds | ✅ | baseline kinds `TurnStarted`, `UsageSnapshot`, `TurnCompleted` present in both; `run_all.sh` "comparable event kinds: PASS". |
| reference determinism (2 runs) | ✅ | `ref1/transcript.txt` and `ref2/transcript.txt` byte-identical; `ref.diff` 0 bytes. |
| capability fallback | ✅ | neither adapter advertises `synthetic_feedback`; driver chooses declared plan, logs `fallback:synthetic_feedback -> fresh_session+handoff`; required optional set empty. |
| cancellation + no orphans | ✅ | `cancel_acknowledged` PASS; `cancel_terminal` PASS (opencode outcome `interrupted` → `Failed`); `cleanup_process_group_gone` PASS; no `opencode serve` descendant after `StopAttempt` (checked with `ps`). |
| privacy check | ✅ | `privacy-check.sh` exits 0 on `src/scenario.rs` + `src/contract.rs`; negative self-test exits 1 on a file containing `/api/`/`127.0.0.1`/`opencode`. |
| decision baseline smoke | ✅ | `s0d --decisions-smoke` 13/13 PASS; native `cargo test` 3/3 PASS (`static/v1`). |
| ownership/ID/capability freeze | ✅ | recorded below; deltas vs `component-contracts.md` listed below. |

### Event transcripts

- reference (`ref1/transcript.txt`; `ref2` identical):
  ```
  TurnStarted
  ToolOutcome ok shell scripted-tool-ok
  UsageSnapshot in=128 out=32 cost=0.000000 completeness=Complete
  TurnCompleted artifact:turn-result
  TurnStarted
  TurnFailed cancelled
  ```
- opencode (`oc/transcript.txt`; usage values are live and vary per run):
  ```
  TurnStarted
  UsageSnapshot in=2964 out=6 cost=0.000000 completeness=Complete
  TurnCompleted artifact:turn-result
  TurnStarted
  UsageSnapshot in=0 out=0 cost=0.000000 completeness=Partial
  TurnFailed interrupted
  ```

All 12 scenario checks PASS under both bindings (`start_attempt`,
`create_session`, `turn1_terminal`, `baseline_event_kinds`,
`usage_completeness_declared`, `cancel_read_midflight`, `cancel_acknowledged`,
`cancel_terminal`, `cancel_ack_distinct_from_termination`,
`capability_fallback`, `required_capability_set_empty`,
`cleanup_process_group_gone`).

### Ownership / tier / capability freeze

Confirmed against `docs/component-contracts.md`:

- **Tier 1 seams** unchanged: `HarnessApi`, `SandboxApi`, `WorkspaceApi`,
  `ExecutionApi`, `LedgerApi`, `TaskQueueApi`. S0d exercises only `HarnessApi`.
- **Capability profile `mvp-baseline`**: required optional-capability set is
  **empty**; the six optional strings (`session_fork`, `synthetic_feedback`,
  `model_switch`, `constrained_generation`, `compaction`, `context_hooks`) are
  declared optional. Both adapters advertise **none** of them (OpenCode may
  advertise only after S0-scope proof). `required_capability_set_empty` PASS.
- **Canonical IDs** frozen and opaque to the scenario: `attempt:<n>`,
  `session:<attempt>:<n>`, `op:<attempt>:turn/<n>`, `artifact:<name>`.
- **Binding record** (config): `binding_id`, `component_api`, `api_version`,
  `implementation`, `implementation_version`, `capabilities`, `config_hash`
  (plus `required_capabilities`/`optional_capabilities` for the driver bind
  check). The driver refuses to bind on API/version mismatch or a missing
  required capability.
- **State ownership**: OpenCode native session IDs (`ses_*`), HTTP, auth, XDG
  rendering, prompt text, and process management live **only** in
  `src/opencode.rs`. The scenario and contract contain none of them (privacy
  check). `runs/` and `target/` are gitignored.

Deltas vs `component-contracts.md` (orchestrator folds these; the doc is
unedited):

- `StopAttempt` returns `StopEvidence { process_group_gone, detail }` so cleanup
  evidence is part of the seam, not inferred by the caller.
- `CancelOperation` returns `CancelAck { acknowledged, terminated, detail }`,
  separating acknowledgment from confirmed termination (matches the doc's
  wording; makes the distinction testable).
- Binding record adds `required_capabilities`/`optional_capabilities` to express
  the `mvp-baseline` profile and the driver-side bind gate.
- `OperationState` has no `Cancelled` variant; a cancelled operation reaches
  terminal `Failed { reason }`. Recorded as a contract question for the product
  freeze.
- Transport is hand-rolled `std::net::TcpStream` HTTP/1.1 (no `curl` process, no
  crates). SSE is **not** parsed.

Decider adapter requirements recorded from S0a: generative inference uses
`HarnessApi` one-shot constrained generation; a specialized adapter runs as a
supervised process with read-only weights, no ledger/Git/tool authority, idle
unload; local resource measurement required before adoption. S0a caveats:
isolated model catalog showed only free provider models; `/api/provider` is not
authoritative for auth; `OPENCODE_API_KEY` did not populate it.

### Deviations and new open items

- **Cancel turn uses a fresh session.** The session projection aggregates
  `outcome` at session scope and does not clear it when a new turn starts, so a
  second turn in the same session has an ambiguous per-turn terminal state. The
  scenario creates a fresh session for the cancelled turn (consistent with
  §6 phase isolation) and the reference adapter mirrors it. The scenario is
  otherwise identical across bindings.
- **Durable session log was empty in isolation.** `GET
  /api/experimental/session/{id}/log` (with and without `follow`) emitted only
  `log.synced`; the server's `event` table had 0 rows while `event_sequence`
  advanced. Reconciliation therefore used the durable session projection
  (`GET /api/session/{id}`: `outcome`, `tokens`, `cost`, `time.idle`) plus the
  log sync marker; SSE was not parsed. This nuances S0a's "reconcile via log"
  note — a follow-on should confirm when/why durable session events persist.
  [open]
- **Process-group signaling EPERM on macOS.** `kill -TERM -<pgid>` returned
  EPERM for the server's own group. The adapter signals the group **and** every
  descendant pid explicitly, reaps the direct child (a zombie otherwise reads as
  alive), then verifies. No orphans remained. `SandboxApi`/`HarnessApi` cleanup
  must not rely on a single group signal. [open]
- **Port-in-use guard.** `start_attempt` refuses to run if the port is already
  serving, so a stale server cannot silently break isolation.
- **HTTP choice recorded:** `TcpStream` hand-rolled client (chosen over `curl`);
  request/response only; the non-`follow` log read closes after the marker.
- **One added module:** `src/json.rs` is a small std-only JSON reader used by
  `main` (config) and the adapter (responses); it has no vendor content. The
  contract still has zero implementation dependencies.
- **Live opencode usage is non-deterministic** (tokens/cost vary); only the
  reference binding is byte-stable. Comparability is asserted on event *kinds*,
  not payload values.
- No git commit/push, no bd writes; only `docs/spikes/s0d-seam-freeze.md`
  Results filled.

### Verdict

- **proceed.** The same scenario passes under both bindings by changing only the
  config file; normalized event kinds are comparable; the reference binding is
  byte-deterministic; capability fallback, cancellation-to-terminal with no
  orphaned process tree, the privacy gate, and all static decision baselines are
  demonstrated. Native HTTP/auth/session/process details are confined to
  `src/opencode.rs`. The tier/capability/ID freeze is confirmed with the deltas
  above. The two open items (durable-log persistence; group-signal EPERM) are
  adapter-hardening notes, not blockers.

