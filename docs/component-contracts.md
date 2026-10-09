# Nano service contracts

Status: architecture requirement. Agalma consists of replaceable components, each
with a versioned API, explicit responsibilities, and owned state. A compatible
implementation can replace any component without rewriting its consumers.
OpenCode is the first harness implementation behind Agalma's `HarnessApi`.

## Boundaries and local deployment

Each nano service exposes domain requests, results, and events through an Agalma
contract. Consumers depend on that contract. Implementation types, database
tables, vendor configuration, native session IDs, and CLI output stay private.
Components receive their dependencies through explicit API bindings; direct
imports of another implementation or access to its private state are forbidden.

The initial deployment is one Rust supervisor with modules communicating through
typed APIs, plus supervised processes where isolation or a different runtime is
needed. In-process calls use the same domain schemas and semantics as process
calls. The initial process transport is framed JSON over stdio or a Unix socket.
Process boundaries can move without changing the domain contract. Nano services
require neither a network server per component nor additional databases.

The contract package defines serializable request/result/event schemas, API
versions, error codes, and capability names. Rust traits implement those contracts
locally. Types in this package contain no implementation handles or vendor SDK
types. A small composition layer registers implementations and wires their APIs;
factory policy does not branch on harness names or storage engines.

## Component map

These are logical boundaries. Several implementations can share the initial
binary while retaining separate APIs and ownership. Replacement ceremony is
tiered (see Contract tiers); the table below lists every component's owner and
implementation regardless of tier.

| Component | Agalma API and responsibility | Initial implementation / private details |
|---|---|---|
| Supervisor and bindings | `SupervisorApi`: lifecycle, describe/health, stage/drain/activate bindings, shutdown, and recovery status | Rust composition root; registrations and process supervision |
| Directives | `DirectiveApi`: submit/version human goals, scope, budgets, acceptance intent, and external-action authority | Versioned directive record and immutable source; bounded by platform constraints |
| Scheduler | `SchedulerApi`: plan eligible work from task, execution, budget, and recorded decision snapshots | Serial Rust scheduler; no backend-specific task queries |
| Policy and budgets | `PolicyApi`: authorize actions from the active directive, enforce platform limits, validate mutations and promotion requirements | Rust guards and counters; directive scope plus protected constraints |
| Durable execution | `ExecutionApi`: start/inspect executions, apply events, schedule activities, signals, deadlines, cancellation, and reconciliation | Seed executor, later bootstrap kernel; transition definitions |
| Ledger | `LedgerApi`: read canonical state, atomically commit transitions/receipts/intents, export/import, backup/restore | SQLite; SQL, transactions, WAL, and physical schema |
| Task backlog | `TaskQueueApi`: task definitions, dependencies, ready candidates, claims, status projections, and export/import | bd adapter; issue bodies, CLI calls, and native IDs |
| Agent harness | `HarnessApi`: attempt/session/turn lifecycle, normalized events, usage, artifact references, reconciliation | OpenCode adapter; HTTP/SSE, configuration rendering, auth, sessions, and nerve hooks |
| Sandbox and process lifecycle | `SandboxApi`: confined launch, process ownership/status, termination, cleanup | macOS Seatbelt backend; profiles and process-tree tracking |
| Workspace and integration | `WorkspaceApi`: isolated checkout, artifact/diff inspection, compare-and-update integration, tag/revert, reconciliation | Git adapter; repository metadata and command details |
| Verification | `VerificationApi`: run directive-derived checks against an identified candidate, return evidence and verdict | Sandboxed command runner; protected tests and logs |
| Evaluation | `EvaluationApi`: evaluator assessments and paired bench results for immutable candidate/baseline inputs | Fixture/history runner and evaluator roles through `HarnessApi` |
| Decisions | `DecisionApi`: bounded choice/scoring requests and normalized results | Static policy or optional inference adapter; model-specific API |
| Genome | `GenomeApi`: load canonical versioned configuration, assemble mutation candidates, publish accepted versions | Git-backed configuration through `WorkspaceApi`; role/prompt/skill formats |
| Memory | `MemoryApi`: retrieve/curate versioned memories and skills with provenance | Rust retrieval through typed ledger search; SQLite FTS indexes private to the ledger backend |
| Telemetry | `TelemetryApi`: record usage/signals, aggregate metrics, build digests and degradation reports | Rust aggregation over ledger APIs; harness payload translation stays in its adapter |
| Provider authentication | `ProviderAuthApi`: scoped provider access and credential lifecycle | External credential proxy; secrets and provider-specific authentication |
| Factory tools | `FactoryToolsApi`: authenticated claim/report/propose/query operations | MCP gateway; protocol encoding and runtime-specific tool registration |

The conductor composes these services and drives the loop. Its scheduling, policy,
and execution responsibilities are exposed through the APIs above. Replacing the
supervisor itself uses a controlled restart with compatible durable state.
The nerve and MCP server are protocol adapters; their responsibilities can be
implemented through another harness's supported hooks and tool interface.

## Contract tiers

Replaceability is the invariant; full replacement ceremony is earned by
replacement pressure. Tier 1 seams carry it at MVP because they already face a
real second implementation, recovery-pinned effects, or self-modification
exposure. Tier 2 components freeze schemas, ownership, and boundaries now and
add replacement procedure when a replacement is actually scheduled.

| Tier | Components | Required at MVP | Added when a replacement is scheduled |
|---|---|---|---|
| 1 — frozen seams | `HarnessApi`, `SandboxApi`, `WorkspaceApi`, `ExecutionApi`, `LedgerApi`, `TaskQueueApi` | Canonical schemas and IDs, operation receipts, binding pins, reconciliation, fail-closed conformance, versioned export/import where durable state exists | Nothing deferred; these seams are exercised at S0, not promised |
| 2 — declared boundaries | `DirectiveApi`, `PolicyApi`, `GenomeApi`, `VerificationApi`, `EvaluationApi`, `DecisionApi`, `MemoryApi`, `TelemetryApi`, `ProviderAuthApi`, `FactoryToolsApi` | Canonical schemas, ownership declaration, `Describe`/`Health`, no private-state access, typed module behind the contract | Capability negotiation, staged drain/rebind, export/import, conformance fixtures |

Day-one invariants, all components:

- Operations record binding ID/version/generation; recovery never silently
  redirects to the currently configured implementation.
- The contract package carries canonical IDs, requests, results, events,
  errors, and capabilities — no implementation handles or vendor types.
- A component's tables, files, CLI output, and native session state are private
  to its owning implementation.
- An architecture test forbids cross-module imports of implementation internals
  and direct access to another component's storage. Extraction later changes
  transport, not dependencies; this test is what keeps that true.

Cost rule: pay for replacement when replacement is scheduled. Negotiation,
hot-swap, and conformance machinery written before a second implementation
exists is speculative generality; tolerating private-state access because
"extraction comes later" forfeits the tenet. The tiers hold both lines.

## Shared contract rules

- **Version and capability negotiation.** Every component provides `Describe`
  and `Health`, reporting API version, implementation version/digest, capabilities,
  readiness, and required dependencies. Bind only compatible versions and the
  capabilities required for the selected execution profile. A replacement must
  preserve behavior as well as message shape. Breaking semantics require a new
  API major version; optional features are advertised explicitly.
- **Canonical identifiers and configuration.** Use Agalma task, attempt, session,
  operation, model, workspace, and artifact IDs. Adapters privately map native
  identifiers. Role definitions, prompts, skills, routing, and required tool
  policies have canonical schemas; adapters render vendor configuration from
  them. Implementation-specific extensions are namespaced and optional. Factory
  phases cannot require an extension absent from their declared profile.
- **Operations and cancellation.** Effectful requests carry stable operation ID,
  input hash, deadline, execution identity where applicable, ownership/lease
  generation, and binding ID.
  Components expose operation inspection/reconciliation. Duplicate delivery must
  return an existing receipt or reconcile the effect; changed inputs under the
  same ID are rejected. Errors distinguish known failure, unsupported capability,
  conflict, and unknown outcome. A retryable error alone does not authorize
  repeating an effect. Cancellation acknowledges the request separately from
  confirmed termination; termination requires process/state evidence.
- **Events and accounting.** Events have operation identity, producer generation,
  monotonic sequence, and a versioned payload. Consumers deduplicate; gaps or lost
  streams trigger inspection/reconciliation rather than guessed completion.
  Usage declares known values, missing fields, and completeness. Unknown spend
  remains unknown. Critical execution evidence is durably recorded before
  dependent dispatch; optional debug telemetry can be dropped explicitly.
- **State ownership.** Each component owns its logical state and versioned export
  format. Durable canonical records are stored through `LedgerApi`; only the
  ledger implementation accesses its database. The atomic commit API accepts
  expected revisions and a typed batch of state changes, events, decisions,
  receipts, and dispatch intents. It commits the whole batch or returns a
  conflict/failure. Calls do not expose SQL or transaction handles and never hold
  a transaction open during external I/O. A replacement ledger must preserve
  these atomicity and durability semantics.
- **Authority and isolation.** API access carries caller identity and scoped
  capabilities. Policy authorizes privileged effects; the corresponding owner
  enforces that authorization. Process launch always passes through `SandboxApi`.
  Harness workers cannot bypass confinement, access the ledger or main checkout,
  or obtain integration credentials. Trusted adapters use scoped APIs to record
  mappings and evidence; they never access another component's private state.
  In-process modules are trusted code under the deployment policy; untrusted
  workers and candidate implementations require OS confinement.
  Replacing policy/sandbox implementations
  follows the active directive and protected platform constraints; API
  compatibility grants no new authority.

## Harness contract and the OpenCode adapter

The baseline `HarnessApi` covers the whole harness integration, including launch
and cleanup. A replacement supplies the following behavior without exposing its
native API to the conductor:

| Operation | Domain behavior |
|---|---|
| `StartAttempt` | Accept workspace handle, directive-authorized role/tool policy, genome and constraint references, resource limits, and operation identity; request confined launch through `SandboxApi`; return an Agalma attempt handle |
| `CreateSession` | Create a fresh phase context from canonical role/model, prompts, skills, handoff artifacts, and factory-tool bindings |
| `RunTurn` | Start a bounded turn, returning an operation handle for asynchronous observation |
| `InspectOperation` / `ReadEvents` | Report running/terminal/unknown state and normalized progress, tool outcomes, usage, errors, and result artifact references; recover after stream interruption |
| `CancelOperation` | Request interruption; return acknowledgment and termination state separately (`CancelAck`); retain enough evidence for reconciliation |
| `CloseSession` / `StopAttempt` | Release session resources; confirm process cleanup through `SandboxApi`, including descendants, returning cleanup evidence (`StopEvidence`) |

Normalized events include turn started/completed/failed, tool outcome, usage
snapshot, intervention request, and runtime failure. Persist needed evidence and
artifact references through the ledger boundary before relying on disposable
runtime state. Permission requests use `PolicyApi`; factory tools use
`FactoryToolsApi`. Vendor transcripts can be retained as opaque debug artifacts;
the conductor consumes normalized facts.

Optional capabilities include session fork, in-turn model switching, context/tool
hooks, synthetic feedback, compaction, and constrained generation. If a capability
is unavailable, the conductor chooses a declared supported plan, such as starting
a fresh session with an explicit handoff. A profile that requires the capability
cannot bind that adapter. Safety requirements have no permissive fallback.
The MVP baseline profile (`mvp-baseline`) requires none of these; a binding
declares required and optional capability sets and the bind gate refuses any
binding whose required set is unmet (S0d).

The OpenCode implementation owns `opencode serve` invocation, version/auth
discovery, its HTTP client and SSE translation, `.opencode` configuration, native
session IDs, and the nerve plugin. Its launcher describes the approved process to
`SandboxApi`; it does not launch outside that boundary. Another implementation
can use a CLI, SDK, or local server and expose the same Agalma API. The rest of
the factory continues to use canonical sessions, events, results, and policies.

## Runtime replacement

Bindings are versioned records: component/API, selected implementation/version,
configuration hash, capabilities, and generation. Durable operations pin their
binding, so replacing a component cannot redirect a retry to another backend.
`SupervisorApi` owns the registry and retains old bindings for reconciliation
and recovery. Activation is a policy-authorized operation with a durable receipt.
This procedure is required for Tier 1 seams at MVP and activates for a Tier 2
component when its replacement is scheduled.

1. Stage a replacement in isolation. Check API/capability compatibility,
   directive-authorized configuration, conformance evidence, state compatibility,
   and resource limits.
2. Mark the affected old binding as draining for new work. Existing operations
   retain it. Let them finish, or explicitly cancel and reconcile their effects
   before moving work. A deadline alone cannot establish cleanup.
3. For disposable runtime state, start a fresh attempt under the new binding. For
   durable owned state, quiesce writers and use the versioned export/import or
   migration contract, preserving IDs, receipts, pending work, and ownership.
   Only one writer may own a given state generation.
4. Atomically record the new binding generation and assign new work to it. Record
   activation evidence. Retain the previous implementation and compatible state
   recovery path until rollback/reconciliation requirements are satisfied.

For a harness change, the boundary is the attempt. New attempts can use a different
harness while existing attempts finish on their pinned adapter. Replacing a
failed adapter during an attempt requires confirmed termination and a new attempt
from durable handoff artifacts. Live vendor session state has no portability
guarantee. In the serial MVP, a swap occurs after the active attempt settles.

Registered in-process implementations or supervised process implementations can
be rebound at these boundaries without restarting the supervisor. Introducing new
compiled code or replacing the supervisor/active ledger requires controlled
deployment or restart. Every component remains replaceable; its lifecycle and
state contract specify the safe activation procedure. Rollback changes future
bindings and reconciles existing effects; it never silently rewinds accepted Git
updates or the live database. Code, genome, and guard changes retain their
directive scope and platform constraints. Routine changes receive no human
review; humans can revise the active directive at any time.

## Acceptance

Conformance fixtures are defined by Agalma's contracts and protected from candidate
edits. Each implementation must demonstrate the required behavior, including
duplicate requests, unknown outcomes, stale reports, cancellation, lost events,
state export/import, and compatible upgrade/replacement where applicable.

The initial harness seam must run the same conductor scenario with the OpenCode
adapter and a deterministic reference adapter by changing only the binding.
This proves API independence; each production harness still needs its own real
tool, confinement, accounting, and recovery evidence. Exercise a harness rebind
between attempts, a cancelled-attempt replacement, and a restart with a pending
operation pinned to the old adapter. Missing required capabilities must prevent
activation. Report replacement latency and resource overhead alongside correctness.

No consumer may parse OpenCode SSE, call its endpoints, read bd CLI output, issue
ledger SQL, or render vendor configuration outside the owning implementation.
New components must declare their API, state owner, dependencies, and replacement
procedure before joining the factory.
