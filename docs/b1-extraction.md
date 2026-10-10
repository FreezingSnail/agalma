# B1 — component extraction design

Bead: `agalma-0ht.1` (B1-D1). Epic: `agalma-0ht` (B1). Orchestrator-authored; gates
all B1 beads. Refs: `docs/durable-execution-bootstrap.md` §Follow-on bootstrap
project, `docs/component-contracts.md` (tiers, runtime replacement, acceptance),
`docs/architecture.md` §13 B1, `docs/b0-exit-evidence.md`,
`docs/spikes/s0d-seam-freeze.md`.

## Goal

Move Tier-1 component implementations out of the conductor binary behind process
(socket) boundaries, one boundary at a time, without changing any Agalma API,
contract schema, or consumer logic. Replacement and rollback then happen through
the declared APIs with dual-binding evidence.

Exit (epic): the component map is realized where replacement pressure exists; every
extraction passed dual-binding evidence, staged rebind, and rollback without state
loss. "Complete when replacement through the declared API is proven, not when a
process was split."

Tier 1 seams: `HarnessApi`, `SandboxApi`, `WorkspaceApi`, `ExecutionApi`,
`LedgerApi`, `TaskQueueApi`. Tier 2 components keep declared boundaries; their
replacement procedure activates only when a replacement is actually scheduled.

## Starting inventory (2026-10-10, HEAD `d0ece01`)

- All component calls are in-process Rust trait calls; no framed JSON transport
  exists anywhere in `crates/`.
- Binding machinery already exists: `BindingRecord` + states
  (`crates/agalma-contracts/src/binding.rs:15`), kernel staged activation,
  rollback, restore (`crates/agalma-kernel/src/activation.rs`), per-operation
  binding pins (`crates/agalma-kernel/src/binding.rs`), durable
  `component_bindings` table (`crates/agalma-ledger/src/lib.rs:109`), conductor
  harness rebind drill (`crates/agalma-conductor/tests/rebind.rs`).
- Bind gate: `harness_binding.rs:172` (`plan_from_describe`), kernel
  `activation.rs:72` (`capability_gate`). `Describe` exists for the harness; no
  `Health` type exists yet.
- Architecture test: src-only cross-impl-crate token check
  (`crates/agalma-conductor/tests/architecture.rs:153`).
- No shadow/parity runner; no component server; most consumer call sites use
  concrete types rather than trait objects.
- Extraction impact: conductor is the only consumer of Workspace/TaskQueue;
  sandbox is consumed by conductor, harness, and kernel; harness op adapter
  already launches a confined `opencode serve` child.

## Decisions

### D1 — Transport: framed JSON over a Unix socket

- Frame: 4-byte big-endian `u32` length + UTF-8 JSON payload; max frame 16 MiB.
- New crate `agalma-ipc` (infrastructure, not an implementation crate): frame
  codec, server runtime, client core. Depends on `agalma-contracts` + serde_json.
- Request `{"v":1,"id":<u64>,"method":"<snake_case method>","params":{...}}`;
  response `{"v":1,"id":<u64>,"result":{...}}` or `{"v":1,"id":<u64>,
  "error":{"code":"<ContractError variant>","message":"..."}}`. Transport faults
  map to `Unavailable`; in-flight effectful calls whose outcome cannot be known
  map to `UnknownOutcome` and are reconciled through existing inspection APIs.
  Duplicate request with same `id` returns the recorded receipt/result.
- Serial MVP: one request in flight per connection; server processes requests
  sequentially. Reconnect allowed; protocol is stateless across connections.
- Readiness: server takes `--socket <path>`, binds, then prints one line
  `READY {json}` on stdout (`api`, `api_version`, `impl`, `impl_version`,
  `capabilities`). Supervisor waits with timeout and runs the bind gate against
  the binding record before use (`plan_from_describe` semantics stay
  authoritative).

### D2 — Process model and crate layout

- Each implementation crate ships a server binary that hosts its `*Api` impl:
  `crates/<impl>/src/bin/<impl>-server.rs` (e.g. `agalma-harness-opencode-server`).
  Server entry = `agalma-ipc` runtime + typed method handlers for that API.
- Typed clients (`HarnessClient`, `SandboxClient`, `WorkspaceClient`,
  `TaskQueueClient`, `LedgerClient`, later `ExecutionClient`) live in
  `agalma-ipc` and implement the same contract trait over the transport.
- Launch spec: each impl crate exports a `CommandSpec { program, args, env }`
  constructor for its server; the conductor registry maps binding
  `implementation` → spec. Registry data, not policy branching on vendor names.
- Component servers are trusted code (`docs/component-contracts.md` §Authority):
  the conductor launches them as supervised child processes (stdio/socket paths
  under the run state dir). OS confinement (`SandboxApi`) is required for
  candidate or untrusted implementations — API compatibility grants no new
  authority (`component-contracts.md` §Authority and isolation).
- A derived requirement: when a dependency boundary is extracted later, the
  already-extracted server switches to that dependency's client (e.g. the harness
  server uses the sandbox client after B1.3). Dependencies stay dependencies;
  extraction changes transport only.

### D3 — Supervision and binding lifecycle

- Conductor-side `ProcessBinding` owns the child (via `SandboxChild`), socket
  path, and generation. Crash restart keeps the same binding id/generation and
  never rebinds an in-flight operation.
- Activation reuses kernel `staged_activate` semantics: set latch → settle
  in-flight → stage candidate → swap binding record → clear latch. "Settle" =
  drain old process: no new operations; finish, or cancel and reconcile, then
  stop the process group with termination evidence.
- Rollback reverses the swap; candidate → `Retired`; prior binding and its state
  recovery path retained. Rollback never rewinds accepted Git or database state.
- Durable-state components (ledger; any future stateful server) use the versioned
  export/import contract while quiesced; only one writer per state generation.
  Snapshot transfer uses files under the run state dir (not frames) and the
  client returns the snapshot path.

### D4 — Shadow parity methodology

Per boundary:

1. Run the boundary's scenario twice against identical scratch state: in-process
   binding vs process binding, same fixtures, deterministic impls where possible
   (reference adapter; fake sandbox; fixture repo). Live OpenCode only where a
   scenario already requires it.
2. Collect a normalized transcript: event kinds and payloads with timing/IDs
   normalized, operation receipts, produced git digest, ledger snapshot
   (volatile fields excluded), `Describe`.
3. A parity checker (test in the extraction bead) fails on structural difference.
4. Artifacts under `target/test-runs/b1/<boundary>/<run>/`; evidence written to
   `docs/b1-<boundary>-evidence.md`, structured like prior exit evidence.

### D5 — Consumer invariance rule

- Contracts unchanged. `agalma-conductor` phase/work logic unchanged.
- Allowed diffs per extraction bead: server bin + `CommandSpec` in the impl
  crate; typed client in `agalma-ipc`; composition/construction sites
  (`composition.rs`, `work.rs`, `harness_binding.rs`, harness construction in
  `phases.rs`); new tests; evidence docs. `git diff --name-only` is part of the
  evidence and must list only these.

### D6 — Architecture test extension

- Keep the existing src-only cross-impl check.
- Add: no implementation crate may list another implementation crate as a normal
  dependency (Cargo.toml edge check); `agalma-ipc` is infrastructure and allowed;
  consumers keep receiving traits.
- Add: extracted component servers must not import conductor internals.

### D7 — Serial order, one boundary per bead

Extractions run strictly one at a time; each boundary's shadow and rebind
evidence builds on the previous. Children are chained with `blocks` deps.
Parallel extraction violates "one boundary at a time" and is not planned.

## Extraction order and rationale

| # | Boundary | Replacement pressure | Crash-containment payoff | Why this position |
|---|---|---|---|---|
| 1 | `HarnessApi` | two implementations today (OpenCode + reference); fastest vendor drift | adapter/vendor faults stop killing the conductor | deterministic reference gives cheap parity; attempt-boundary rebind already defined and drilled (M1.7, B0 case 9) |
| 2 | `SandboxApi` | container sandbox deferred; profile evolution | process-tree leaks/faults isolated; serves every confined launch | small surface; harness server switches to the sandbox client here |
| 3 | `WorkspaceApi` | low (git stays git) | git subprocess faults contained | small surface, single consumer |
| 4 | `TaskQueueApi` | bd CLI churn; future intake adapters | bd faults contained | small surface, single consumer |
| 5 | `LedgerApi` | Postgres deferred; backup/restore operations | DB faults/restarts isolated from the conductor | durable state: export/import + single-writer proven on mature transport |
| 6 | `ExecutionApi` | seed executor vs kernel; future runners | kernel faults contained | largest boundary; done last with everything proven |

## Per-extraction recipe (each bead)

1. Server binary hosting the existing impl in the owning crate; `CommandSpec`.
2. Typed client in `agalma-ipc` implementing the contract trait.
3. Conformance tests at the IPC level: duplicate request id returns recorded
   result; unknown outcome surfaces as `UnknownOutcome`; connection loss mid-call
   reconciles; frame limit enforced; version mismatch refused.
4. Shadow parity run per D4.
5. Staged rebind **at the boundary's natural point** (harness: between attempts;
   stateless components: between operations; ledger: quiesced export/import),
   with an in-flight operation pinned across the swap.
6. Rollback test without state loss.
7. Evidence doc + `git diff --name-only` invariance proof + architecture test
   green.

A boundary bead is complete when replacement through the declared API is proven:
same scenario passes under in-process and process bindings, rebind/rollback
evidence recorded, no consumer-code diff.

## Bead plan

| Bead | Scope |
|---|---|
| `agalma-0ht.1` | B1-D1 design (this doc) — gates all B1 beads |
| `agalma-0ht.2` | B1.1 IPC foundation: `agalma-ipc` crate, framing, server runtime, client core, conformance harness, architecture-test extension |
| `agalma-0ht.3` | B1.2 Harness extraction (first boundary) |
| `agalma-0ht.4` | B1.3 Sandbox extraction (+ harness server → sandbox client) |
| `agalma-0ht.5` | B1.4 Workspace extraction |
| `agalma-0ht.6` | B1.5 TaskQueue extraction |
| `agalma-0ht.7` | B1.6 Ledger extraction |
| `agalma-0ht.8` | B1.7 Execution/kernel extraction |
| `agalma-0ht.9` | B1.8 exit evidence: component map realized, aggregate evidence, epic close |

## Risks / open items

- Snapshot size vs 16 MiB frame limit: ledger export/import uses file paths, not
  frames (D3); revisit if another API transfers large payloads.
- Sandbox profile for component servers must allow loopback for the harness
  server's own child (`opencode serve`); profile work belongs to B1.3/B1.2.
- `KernelEngine` does not implement `ExecutionApi` today (only
  `Executor<L>` does); B1.7 must resolve the actual replaceable boundary before
  extraction.
- Timing-sensitive tests: parity checker normalizes timestamps/IDs; any residual
  nondeterminism is documented per boundary rather than hidden by retries.
