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

### Exit criteria

| Item | Status | Evidence |
|---|---|---|
| both bindings, config-only flip | | |
| comparable event kinds | | |
| reference determinism (2 runs) | | |
| capability fallback | | |
| cancellation + no orphans | | |
| privacy check | | |
| decision baseline smoke | | |
| ownership/ID/capability freeze | | |

### Event transcripts

- reference:
- opencode:

### Deviations and new open items

- ...

### Verdict

- proceed / fallback / kill:
