# Agalma — Architecture

Status: draft v0. Captures the design decisions from planning. Questions still open
are marked **[open]**.

Agalma is an always-running software factory. It evaluates, builds, and ships its
target — and never stops. The first milestone is a bootstrapping MVP that can
begin to improve itself.

The factory is composed of replaceable **nano services**: components with siloed
responsibilities, owned state, and versioned Agalma APIs (§2.1). A component can
be replaced by any implementation that satisfies its API and behavioral contract.
The initial deployment has two principal parts:

- **Agalma (Rust)** — the conductor: queue, scheduling, budgets, integration,
  evaluation, evolution. This is the product.
- **Harness adapter, initially OpenCode v2** — implements Agalma's `HarnessApi`
  for runtime launch, model calls, tools, sessions, events, and cleanup. OpenCode
  runs as `opencode serve`; another compliant harness can replace this adapter.

Durable execution lives inside the Rust conductor, backed by SQLite. Agalma
builds its own local execution kernel, using Temporal's durable history and
workflow/activity separation as design references. A minimal executor stands up
the factory; evolving it into the kernel is an early factory bootstrap project
(§3.6, §13). Temporal deployment and adoption of Apalis are deferred.

Decider models provide optional, bounded inference for routing and other choices
(§3.7). The conductor supplies eligible actions, validates the result, and records
the decision before acting. Static policies let the initial factory run without a
resident decision model or another service.

The OpenCode adapter drives its supervised child over HTTP (REST + SSE), with a
thin TypeScript plugin ("nerve") bridging control points inside that server.
These details stay inside the adapter. The conductor uses Agalma's harness
contract, independent of the selected runtime's API or deployment method.

---

## 1. Purpose and scope

**Goal:** build a self-improving factory from replaceable components, while making
cheap models outperform their price class through harness engineering. OpenCode
is the first harness implementation.

MVP scope: a local tool whose target is the Agalma repository itself. Single
machine, single conductor, serial execution first. The factory must reach a state
where it proposes, evaluates, and promotes improvements to its own genome unattended,
then autonomously improves and ships code against a human-provided directive.
The destination has no human review or per-change approval gate. Humans set or
revise the goal, scope, resource budget, and external-action authority through
versioned directives; the factory evaluates, corrects, or reverts each change
against that directive and its protected platform constraints.

Non-goals for the MVP: multi-target support, deployment roles, container/VM
sandboxing, dashboards, model fine-tuning.

### Non-negotiable principles

1. **Sessions are cattle; the ledger is truth.** Sessions may die, be
   interrupted, or be recreated. The durable record of what happened lives in
   Agalma's SQLite ledger, including execution state and operation receipts.
2. **Deterministic Rust where possible, models where judgment is needed.**
   Scheduling constraints, gating, merging, score aggregation, and accounting
   are code. Decider models handle bounded classification, selection, and ranking.
   Creation, design, and independent quality assessment are agents/services. The
   factory makes the release decision from recorded evidence; humans provide the
   directive rather than reviewing each result.
3. **Mechanical guarantees over prompt promises.** Gates are counters, exit
   codes, schema checks, and file hashes — not requests to the model.
4. **Everything is a task; everything is versioned; everything is revertible.**
5. **Cost is a first-class outcome.** The headline metric is cost per merged
   change at a fixed success bar.
6. **Harness > model.** The factory's evolutionary pressure optimizes the
   harness, not the model weights.
7. **Components are replaceable nano services.** Each component has an explicit
   versioned API, state owner, and lifecycle. Consumers depend on the contract;
   implementation details stay siloed. Compatible replacements bind without
   rewriting consumers. Runtime swaps occur at declared safe boundaries.

---

## 2. Runtime topology

```
agalma (Rust supervisor + nano service composition and bindings)
├── SupervisorApi → lifecycle, registry, staged component activation
├── SchedulerApi / PolicyApi → Rust scheduling, budgets, guards
├── ExecutionApi → embedded seed executor, later bootstrap kernel
├── LedgerApi → SQLite state, jobs, decisions, runs, operation receipts
├── HarnessApi → OpenCode adapter (initial implementation)
│     ├── SandboxApi → confined opencode serve child per attempt
│     ├── private HTTP/SSE client, native sessions, configuration renderer
│     └── private nerve plugin → normalized hooks/events and policy requests
├── SandboxApi → Seatbelt process lifecycle
├── WorkspaceApi → Git checkouts, artifacts, integration
├── TaskQueueApi → bd backlog adapter
├── DecisionApi → static policy or optional inference adapter
├── DirectiveApi → versioned human goals, scope, budget, external-action authority
├── GenomeApi → canonical configuration, mutation candidates, accepted versions
├── VerificationApi / EvaluationApi → command checks, rubric and bench
├── MemoryApi / TelemetryApi → retrieval, usage, metrics, digests
├── ProviderAuthApi → credential proxy
└── FactoryToolsApi → agalma-mcp gateway and tool bindings
```

- One disposable harness runtime per attempt, with fresh sessions for its phases.
  The OpenCode adapter initially uses a server. On macOS `SandboxApi` launches
  the entire worker under Seatbelt via `sandbox-exec`, so built-in file tools and
  subprocesses share the boundary.
- A dedicated child process is used instead of the user's shared background
  service: the factory owns the runtime lifecycle and version, and never fights
  the human's TUI for config or state. Visibility for the human is a mirror
  concern, solved by ledgers and digests, not by sharing the service.
- Attempts use independent checkouts with their own Git metadata, rather than
  linked worktrees that share the main repository's metadata. The conductor
  prepares the checkout through `WorkspaceApi`; authorized integration through
  that API is the only writer to the main checkout.
- Execution state and scheduling deadlines are stored in a local SQLite file.
  Job workers run inside the conductor; the local MVP requires no workflow
  server or database service. The factory owns execution state, dispatch tables,
  and their canonical contracts; the ledger implementation owns its physical
  schema and database access.
- Crash semantics: harness runtime crash = attempt runtime crash. The conductor
  recovers execution progress through `ExecutionApi`/`LedgerApi`, reconciles
  receipts with the pinned component, and terminates surviving processes through
  `SandboxApi` before retrying or parking. Native session state is disposable
  cache, not a recovery prerequisite.

### 2.1 Nano service boundaries and replacement

The [component contracts](component-contracts.md) define each API, its state owner,
and replacement procedure, and tier the replacement ceremony: seams that face a
real second implementation or recovery-pinned effects carry full replacement
discipline at MVP; other components freeze schemas and ownership now and add
versioned export/import, negotiation, and conformance when a replacement is
scheduled. The local MVP uses Rust modules behind typed contracts
and supervised processes where needed. The same schemas and semantics support
local process calls; a nano service does not require a separate network server.

- API definitions use canonical Agalma IDs, configuration, requests, results,
  events, errors, and capabilities. Consumers cannot import another implementation,
  read its private state, or depend on vendor payloads.
- A binding registry selects compatible implementations. Operations pin binding
  ID/version/generation, input identity, and lease generation for recovery.
- Hot swaps direct new work to a compatible replacement after staging and checks.
  Existing work retains its old binding until completion or cancellation and
  reconciliation. Harness replacement occurs between attempts; live native
  sessions are not a transferable contract.
- Durable state replacement requires exclusive ownership, a versioned migration
  or export/import, preserved IDs/receipts, and a compatible rollback path.
  New compiled code, supervisor replacement, or active ledger replacement uses
  controlled deployment/restart where required.
- Conformance includes behavior: cancellation, idempotency/reconciliation,
  accounting, state compatibility, and required safety capabilities. Matching
  message shape alone is insufficient. Component changes must pass the active
  directive, protected platform constraints, and autonomous promotion gate.

Replacing the component that launches OpenCode means binding another `HarnessApi`
implementation. Phase orchestration, task intake, verification, integration,
decisions, and evaluation continue using their existing Agalma contracts.

---

## 3. Layers

### 3.1 Conductor (Rust)

The control plane composes nano services through their APIs. Policy retains
authorization authority; implementations receive scoped capabilities for their
effects. Responsibilities:

- backlog intake and claim reconciliation through `TaskQueueApi`; scheduling
  and concurrency control through `SchedulerApi`/`PolicyApi`
- directive intake and versioning through `DirectiveApi`; derive pinned task
  acceptance criteria and permitted actions
- deterministic state transitions for phase orchestration per task
  (isolated checkout → build → verify → evaluate → integrate)
- activity dispatch to the owning component APIs for process, filesystem,
  persistence, harness, and Git effects; operation receipts, reconciliation,
  lease fencing, and cleanup
- bounded decision requests: eligible choices, inference dispatch, result
  validation, static fallback, and durable decision records
- watchers on normalized harness events (repeat loops, failure streaks, budget)
- attempt/session lifecycle through `HarnessApi`, process cleanup through
  `SandboxApi`; optional capabilities negotiated before use
- deterministic verification (build/test/lint commands and exit codes)
- integration: merge, tag, rollback through `WorkspaceApi`
- bench invocation and evidence-based promotion decisions under the active directive
- cost accounting, digest generation, degradation detection
- kill switch

### 3.2 Harness nano service (initially OpenCode)

`HarnessApi` owns the integration with the selected agent runtime: confined
attempt launch, canonical role/configuration rendering, session/turn lifecycle,
normalized events and usage, artifact references, cancellation, and reconciliation.
The initial OpenCode adapter maps these operations to the implementation below.

Owns everything inside an agent turn:

- agent loop: system prompt assembly, model calls, tool-call parsing and
  execution, retries, compaction
- standard tools (read/write/edit/bash/grep…) executed against the session's
  working directory
- permission framework, sessions/inbox, worktrees, skills, LSP diagnostics,
  MCP client
- provider configuration for generative model calls; specialized decider inference
  can use a separate `DecisionApi` implementation (§3.7)

OpenCode adapter endpoints at MVP (private hand-rolled async Rust client; generate
later from the spec; corrected against v2.0.15 in S0a): `GET /api/info`,
`/api/model`, `/api/provider`, `/api/agent`, `POST /api/session`,
`POST /api/session/{id}/prompt|interrupt|model|agent|synthetic|compact`,
`POST /api/session/{id}/shell`, `POST /api/experimental/session/{id}/wait`,
`GET /api/session/{id}/diff`, `GET /api/experimental/session/stats`,
`GET /api/experimental/session/{id}/log?follow=true`, `POST /api/location/reload`,
project-scoped `/api/worktree` list/create/refresh/delete, `GET /api/event` (SSE).
Server auth is Basic (user `opencode`); the password comes from `OPENCODE_PASSWORD`
or the service file — there is no `--password` flag. SSE is volatile by contract;
reconciliation uses the durable session log or the idle barrier, never assumed SSE
continuity. S0d found the durable log emitting only a sync marker in isolated runs
(server event table empty); until its persistence is confirmed, reconciliation
falls back to the session projection. [open] Full observations:
`docs/spikes/s0a-harness.md`, `docs/spikes/s0d-seam-freeze.md`.

### 3.3 OpenCode nerve plugin (TypeScript)

A thin plugin loaded into the OpenCode server. It exists because hooks and
transforms are only available to in-process plugins, and the plugin API is
TypeScript-only. It is private to the OpenCode adapter, translating hooks into
canonical events and requests to Agalma APIs. Another harness provides equivalent
advertised capabilities through its own integration.

| Hook | Duty |
|---|---|
| `tool.execute.before` | tool-input repair (schema-driven); path/argument validation |
| `tool.execute.after` | signal capture: errors, repairs, commands, outcomes |
| `context` | inject memory snapshot, top bd issues, steering fragments; drop tools |
| `compaction` | cheap-model summarizer via one-shot generation |
| `permission.evaluate` | bridge to Rust policy decision point |
| `retry` | retry/backoff policy from Rust |
| transforms (later) | override `edit`/`read` with hash-anchored / minified variants |

Boundary rules:

- The nerve holds no business logic and no durable state. Agalma APIs own policy
  and durable records; the OpenCode adapter normalizes hook payloads.
- Bridge transport: Unix socket, JSON messages. Low traffic, local only.
- Optional repair and telemetry hooks may no-op if Rust is unreachable.
  Authorization decisions must fail closed. Filesystem confinement does not
  depend on the nerve; Seatbelt is applied before the worker starts. Privileged
  factory operations require conductor authorization through a narrow bridge.
- The nerve is versioned with the OpenCode adapter. Its configuration is rendered
  from the pinned genome; reload via `POST /api/location/reload` is private to the
  adapter. Consumers request advertised capabilities through Agalma APIs.

**[resolved in S0a]** `tool.execute.before` can both mutate `event.input` in
place and block a call by failing the hook; execution uses post-hook input.
See `docs/spikes/s0a-harness.md`.

### 3.4 agalma-mcp (Rust MCP server)

Adapter for `FactoryToolsApi` and a language-neutral path for model-facing tools.
Provides factory tools (`agalma_claim`, `agalma_report`, `agalma_propose`,
`agalma_query`) and the
memory search surface through `MemoryApi` to sessions. The initial harness adapter
renders MCP configuration; another harness can bind the same factory-tool API
through its supported tool protocol. MCP encoding stays inside the gateway.

MCP is for tools the model calls. The nerve is for control points Agalma needs.
Different jobs.

### 3.5 Genome

`GenomeApi` owns the factory's canonical, versioned operating configuration and
mutation candidates. Its mutable data includes:

- canonical role definitions — researcher, planner, builder, verifier, evaluator,
  postmortem, including per-role model assignment; the harness adapter renders
  `.opencode/agents/*.md` or its own native equivalent
- skills — procedural memory, curated and written by the factory itself
- prompt fragments — model-aware steering, finish discipline, phase prompts
- routing table — role → model, escalation ladder; decision kind → decider
- decision templates and selection/abstention thresholds — adjusted within the
  active directive's objective, model budget, and protected action vocabularies
- retry and escalation settings — adjusted within platform reliability limits

Platform constraints are versioned independently from the mutable genome. They
include sandbox permissions, protected paths, credential scope, hard spending
caps, and evaluator integrity. The active directive cannot weaken these constraints.
`DirectiveApi` versions human changes to objectives, task scope, acceptance intent,
resource budgets, and permitted external actions. The conductor validates genome
and code changes against the pinned directive and platform constraints; a
candidate cannot rewrite its own pinned acceptance criteria or evaluator.

Genome releases are git tags (`genome/vN`). Every run records the genome hash,
so performance is attributable to a specific genome. New sessions receive their
pinned genome through `HarnessApi`. Configuration reload is an advertised adapter
capability; callers do not write vendor configuration or call reload endpoints.

### 3.6 Local durable execution (Rust + SQLite)

`ExecutionApi` exposes an explicit task/phase state machine persisted through
`LedgerApi`, initially implemented with SQLite. Each task execution has a stable
ID derived from target, canonical Agalma task ID, and an execution generation.
A deliberate rerun gets a new generation; duplicate starts
of the same execution reconcile the existing record. Phase boundaries are durable
checkpoints with immutable references to handoff artifacts. `TaskQueueApi` maps
canonical task IDs to bd issues; its execution status is a projection of the
ledger. Replacing the backlog implementation preserves those canonical IDs.

The implementation is an internal native Rust nano service behind `ExecutionApi`,
using atomic typed commits through `LedgerApi`. Temporal's
[durable event history](https://docs.temporal.io/encyclopedia/event-history) and
[idempotent activities](https://docs.temporal.io/activity-definition) inform the
design: orchestration decides what to schedule; activity handlers perform I/O;
persisted inputs, outcomes, and deadlines determine recovery. This is an independent
implementation of the local contracts, with no Temporal SDK/server dependency or
wire compatibility requirement.

M0 supplies a minimal serial executor for the fixed factory phases, durable
operation receipts, restart reconciliation, and a kill switch. After M1, the
factory develops this executor into a reusable internal kernel through the
[durable execution bootstrap project](durable-execution-bootstrap.md). The
existing accepted executor runs the project; candidate kernel code runs against
isolated databases and fake or sandboxed activity handlers. The project improves
the runner only after a working runner exists. The [alternatives assessment](orchestration-options.md)
is retained as research, superseded by the decision to build internally.
A follow-on bootstrap project (B1) extracts modules behind process or socket
boundaries where replacement pressure exists, per the contract tiers. Like B0 it
runs under transitional oversight, with dual-binding shadow runs and staged
rebinds before activation.

The kernel uses explicit versioned states and commands. A deterministic transition
function derives the next commands from recorded events. Recovering history must
not perform I/O; it reconstructs state and then reconciles pending operations.
Completed activities return their recorded results. Pending activities may be
delivered again and must reconcile external effects. Durable execution cannot
promise exactly-once external actions without cooperation from those systems.
One dispatch handles one logical phase/operation. General async-function replay,
distributed execution, and a standalone workflow platform are outside this project.

Execution state, state-transition events, operation receipts, and durable dispatch
intents are committed in one `LedgerApi` transaction. Database handles and SQL
stay inside the ledger implementation. Dispatch intents survive a crash between
updating phase state and enqueuing its job. Queue insertion may be repeated;
handlers check the logical operation ID and current execution state before acting.
Completed or stale jobs cannot repeat completed work or advance the phase.

Persist retry counts, next-attempt times, deadlines, cancellation requests, and
directive updates. Runtime timers are wakeups: after restart the conductor loads
the stored deadlines. Directive records bind to a version, source, target scope,
and platform constraint version. An updated directive can supersede queued work
or cancel in-flight work; it never silently authorizes a changed candidate.
`PolicyApi` owns authorization, budgets, sandbox lifecycle, and integration.

Long-running handlers heartbeat. Cancellation explicitly interrupts sessions and
terminates their process trees. Job timeout or lease expiration alone is not proof
that a harness process has stopped. Retries reconcile the existing phase before
dispatching; creating a fresh model attempt is an explicit, budgeted state-machine
decision. Queue orphan recovery must pass through this reconciliation.

Dispatch, merge, promotion, and bd updates use stable operation IDs derived from
task execution and logical step, unchanged across delivery retries. The conductor
records intent before acting, then completion afterward. Operation IDs are unique;
reuse with different inputs is rejected. Receipts include pinned inputs and result
references; integration records expected main SHA, candidate SHA, and resulting
commit/tag. On retry, the conductor inspects receipts and actual Git/process/bd
state, returns an existing result if complete, retries only when effects are known
absent, and parks an ambiguous operation. Git ref updates require the expected
old SHA.

The serial MVP holds an exclusive conductor lock. Each claim has an increasing
lease generation; bridge reports and privileged operations must match the current
generation. Before redispatch, the conductor confirms prior workers have stopped.
Expired leases initiate reconciliation, not an unconditional return to ready.
A merge completed before a crash is recorded and reflected in bd without merging
or rebuilding the task again.

Executions pin genome, guard, execution-definition versions, and component bindings.
Recovery invokes the original compatible binding; a configuration change cannot
silently redirect a pending effect to a replacement. Schema migrations
are explicit; upgraded conductor code must understand the stored execution version
or park it for migration. Self-modification cannot silently reinterpret in-flight
state under changed orchestration rules.

The ledger uses SQLite WAL with synchronous durability configured for committed
state (`synchronous=FULL`), short transactions, and a bounded busy timeout. External
processes and model calls never run inside a database transaction. Failure to
persist blocks dispatch and privileged operations. State, WAL files, and backups
remain outside worker access. Backup and restore use SQLite's supported backup
mechanism or a cleanly stopped database; copying the live database file alone is
insufficient. Restore reconciles recorded state with actual Git and bd before work
resumes. Retention must preserve receipts needed by nonterminal executions.

MVP deployment remains a single machine, with no availability guarantee during
machine failure. Kernel changes use transitional human oversight until P1.
Activation occurs at a controlled conductor restart after gates,
history compatibility, and schema compatibility are checked. The accepted runner
is retained for rollback; reverting a binary does not revert the live database.
Candidates requiring an incompatible schema change need an explicit migration and
recovery plan. Platform constraints remain protected from factory changes; a human
changes goals or authorized scope by issuing a new directive.

### 3.7 Decider models

A decider answers a bounded question by choosing an option or scoring supplied
candidates. The [Strands Decider example](https://strandsagents.com/blog/introducing-strands-decider/)
illustrates this model class. Agalma's decision interface supports interchangeable
models; specific weights, SDKs, and inference runtimes are candidates to evaluate.

| Decision kind | Decider contribution | Conductor constraints |
|---|---|---|
| Task classification and model routing | Classify task family/complexity; select a role/model route | Evaluated routes; capability, context, cost, and availability checks |
| Failure triage and next action | Classify failure; choose repair, diagnosis, retry, escalation, or park | Mechanical triggers, remaining attempts, budget, deadlines, and allowed phase transitions |
| Memory and context selection | Rank retrieved memories, skills, or steering fragments | Approved retrieval scope, mandatory instructions, provenance, and context limits |
| Task ranking | Rank ready tasks within an eligible priority group | Dependencies, human priorities, fairness rules, claims, and resource limits |
| Advisory tool checks (later) | Classify suspected ungrounded arguments or irrelevant calls; recommend guidance | Supported nerve hooks; schema, permission, and sandbox enforcement in Rust |

**Decision contract.** Rust constructs a versioned request with a stable operation
ID, decision kind, bounded context or immutable artifact references, eligible
option IDs, deadline, and pinned policy/genome/model/adapter versions. Options are
filtered before inference. A single eligible option needs no model call. If there
are no eligible options, Rust waits or parks. Responses contain a
chosen ID and/or finite scores over the supplied IDs, with optional confidence
and its declared meaning. Rust validates IDs, score ranges, schema, and current
eligibility before applying a result.

The model ranks or recommends within that contract. Rust owns authorization,
spending limits, cancellation, acceptance, integration, and promotion. Required
escalations and circuit breakers remain mechanical. A decider cannot bypass a
mandatory escalation, grant authority, admit an unevaluated route, or remove
mandatory context. Complex diagnosis and rubric assessment remain agent/evaluator work.

**Fallback and uncertainty.** Each decision kind has a versioned static baseline
and bounded inference time/cost. Timeout, unavailable inference, malformed output,
or abstention uses that baseline, rechecking eligibility; if the baseline cannot
act, wait or park. Late responses cannot supersede an applied fallback. Confidence
is optional: raw scores and generated self-confidence are not assumed calibrated.
Use confidence thresholds only after evaluation on Agalma's labeled decisions;
record score semantics and calibration version. Initial routing uses static
policies, then shadow decisions, then an evaluated active route.

**Inference adapters and local footprint.** The interface supports specialized
choice/scoring models and constrained generative models. Generative inference
uses `HarnessApi`; a specialized model may require a separate local adapter, since
its output API need not support text generation. Compatibility is proved rather
than assumed. All implementations expose `DecisionApi`. An optional local adapter
runs as a process supervised through `SandboxApi`, with read-only weights,
bounded inputs/outputs, and no ledger, Git, or tool
authority. Hosted adapters use scoped provider access. Local weights load on
demand with an idle unload policy; residency is an explicit configuration choice.
Measure cold/warm latency, peak/idle memory, CPU/GPU use, and total task economics
before adoption. No additional always-on service is required by the MVP.

**Durability.** Inference is a named I/O activity, outside the deterministic
transition function. Persist the request before dispatch and the validated choice
or fallback plus its receipt before scheduling dependent work. Recovery reuses a
completed decision without asking the model again. Pending inference follows an
explicit bounded retry/fallback policy; a new paid invocation is recorded as a
new attempt. A changed input or superseded state needs a new request, never reuse
of an operation ID with different contents. Recheck guards and eligibility before
the resulting action, including after restart.

**Evaluation.** Maintain protected labeled examples per decision kind and test
invalid outputs, timeouts, stale results, and fallback behavior. Compare accuracy,
calibration where applicable, routing regret against benchmarked alternatives,
and downstream success/$/merge against static policies. Include decision inference
cost, latency, and local resource overhead in the comparison. Promote decision
templates and routes through the existing genome gates; changes to the adapter
or conductor are code changes under the P1 autonomy gate and active directive.

---

## 4. The loop

```
recon → mine/claim task → pin directive + genome → isolated checkout → build → verify → evaluate
  → integrate → record + score → propose mutations → bench gate → promote
```

- The loop never terminates. When the queue is empty, recon and task-mining
  refill it; if that yields nothing, the factory runs bench work and mining on
  history.
- Each phase is a separate session with a fresh context. Handoff happens
  through files (spec, design, diff, test output), never through conversation
  memory.
- Failure path: attempt fails → postmortem note → retry with feedback
  (bounded attempts) → escalate model tier → park with diagnosis. Parked tasks
  surface in the digest.
- Escalation is ladder-driven and mechanical: attempt count, verification
  results, repair failures, and error streaks trigger it. Deciders can select an
  eligible next action or route after those constraints are applied (§3.7).

---

## 5. Task model

A task is a canonical `TaskQueueApi` record. The initial bd adapter represents it
as an issue plus a machine-readable block in its body (fenced YAML):

```yaml
directive: directive/vN
target: agalma            # repo
ref: genome/v7            # base
acceptance:               # commands that must pass, run by Rust
  - cargo test
  - cargo clippy -- -D warnings
budget_usd: 2.00
priority: normal
family: bench|feature|fix|mutation
```

Lifecycle: `ready → claimed → building → verifying → evaluating → integrating →
done | parked`. Claim intent and execution ID are recorded before dispatch;
interrupted starts are reconciled using that identity. `ExecutionApi` owns active
execution state, projected through `TaskQueueApi` by idempotent operations. Claims
are fenced leases; expiration initiates reconciliation under §3.6 before a task
can become ready again. Each task carries its source directive and constraint
version. Creators: directive intake, planner sessions, postmortem mining, bench
failures, recon.

---

## 6. Sessions and phases

- Fresh session per (task, phase, attempt). Context isolation keeps attempts
  comparable and prevents drift; dirge's experience with phase isolation
  supports this.
- Phase roster: recon, plan, build, verify (agent-side diagnosis only),
  evaluation, postmortem.
- `HarnessApi` provides fresh sessions, bounded turns, normalized observation,
  results, cancellation, and attempt cleanup for every implementation.
- Optional capabilities used deliberately when advertised:
  - session fork — explore alternative approaches in parallel
  - constrained generation — cheap one-shot summarization, rubric judging,
    or a generative adapter for bounded decisions
  - synthetic feedback — verifier feedback or interventions mid-run
  - model/role switching — mid-session escalation
  - context/tool hooks and compaction — harness interventions
- Required cancellation and result/usage observation are baseline contract
  behavior. The conductor chooses a supported plan for optional capabilities;
  phase logic never calls OpenCode endpoints directly.

Decider inference has its own request/result lifecycle (§3.7); specialized
choice/scoring inference can run without creating an agent conversation.

---

## 7. Evaluation

Verification happens at increasing levels of trust:

| Level | What | Who |
|---|---|---|
| L0 | build, tests, lint, typecheck; tests from a pristine checkout | Rust |
| L1 | task acceptance criteria (hidden tests where possible) | Rust |
| L2 | evaluator assessment of diff against directive and task spec | evaluator agent |
| L3 | bench regression pack, paired candidate vs baseline | Rust + sessions |
| L4 | economics: tokens, cost, wall time per merged change | Rust |

Fitness: `merge rate − λ·cost − μ·time`, with regression as a veto.

Statistical rules: K trials, paired same-task comparisons, promotion only past
a noise margin; screening uses small K; variance is reported, never hidden.

Bench composition:

- hand-written fixture tasks with hidden tests (seed: 10 tasks, 2 trials)
- history-replay tasks: revert an old commit, the factory must re-land it, the
  original tests are the oracle

Anti-gaming: bench runs from a pristine ref; tests are read-only during runs;
holdout tasks rotate; cost is computed from real usage.

Rejected candidates are stored with reasons ("reject memory") and consulted by
future proposals.

---

## 8. Self-improvement

Mutation scope ladder:

1. **M1 — data**: prompts, skills, routing, retry and escalation settings
   within the active directive and protected platform limits.
2. **M2 — code**: pipeline (Rust) code, including self-modification behind
   `ExecutionApi`, controlled supervisor replacement, and automatic rollback.
3. **Platform constraints**: sandbox permissions, protected paths, hard spending
   caps, credential scope, and authorization limits. The active directive cannot
   weaken them. The human can issue a new versioned directive to change goals,
   scope, task budgets within platform caps, or select externally allowed actions;
   acceptance and evaluator criteria are derived from and checked against that
   directive.

Promotion pipeline (every candidate, no exceptions):

```
candidate diff → directive/constraint check → L0/L1 → independent evaluator
  → paired bench (K trials) → promote (commit + tag) | reject/revert (memory)
```

Autonomous destination: the factory promotes genome, kernel, and target code when
all deterministic checks, independent evaluation, and paired bench criteria pass.
No person reviews or approves a routine change. At startup, humans may temporarily
supervise code integration while the system builds the required recovery and
self-correction evidence. P1 is the explicit transition to autonomous code
integration, after the gates in §13 pass. Rollback is a Git revert plus a routing
revert; detected regressions trigger it automatically.

### 8.1 Directive and self-correction

`DirectiveApi` accepts versioned human directives: the outcome sought, target and
scope, acceptance intent, task budgets within platform caps, and permitted external
actions within platform authority. The factory translates a directive into task acceptance criteria and evaluator
fixtures, records its source version through each task and decision, and tests
changes against those criteria before integration. A human can replace or extend
the active directive at any time; the system does not wait for per-change review.

When a check or production signal fails, the factory diagnoses the failure,
attempts bounded repairs, reruns the same pinned criteria, and promotes only a
passing result. Otherwise it reverts or rejects the candidate, records the reason,
and continues with other work. It cannot self-edit platform constraints or grant
itself external authority. Work outside the directive's scope or authority is
parked and reported as a request for a new directive, not held for approval of a
completed change.

Application of promoted genome changes: publish the canonical version and bind it
to new sessions; the harness adapter handles native rendering and supported reload.

---

## 9. Model onboarding and metrics

Policy — bench once per (provider, model, variant) tuple:

- Screen new arrivals automatically (small bench subset, cents).
- Evaluate automatically within the directive's resource budget. A candidate that
  exceeds that budget is skipped or left on the static route until the human
  changes the directive; evaluation does not wait for per-candidate approval.
- A win produces a routing-table mutation and goes through the normal
  promotion gate.
- No periodic re-evaluation. Production telemetry is the continuous canary.
- Re-bench only on trigger: new directive, provider-announced version bump, or
  degradation alert. A new ID or variant is a new model — registration, not
  re-benching.

Deciders are enabled per decision kind after screening on labeled examples.
Full evaluation includes downstream task quality and local resource caps, plus
total inference overhead. Zero provider charges alone do not establish that a
local model is cheaper. Enabling another decision kind requires its own evaluation,
even when the model already has approval for routing.

Generative model results are scoped to the harness capability/configuration
profile used in evaluation. A replacement harness establishes route evidence for
its profile; prior evaluation does not establish equivalent quality or economics.
Record harness bindings in runs and compare production cells within that profile.

Metrics ledger (SQLite):

```
models(provider, model, variant, context, capabilities, cost_json,
       model_revision, adapter, adapter_version, decision_kinds,
       first_seen, last_eval, status)
runs(run_id, task_id, attempt, phase, agent, provider, model, variant,
     execution_id, harness_binding_id, genome_hash, started, ended, outcome, tokens_in, tokens_out,
     cached_in, cache_write, cost_usd, wall_ms, tool_calls, compactions,
     context_peak)
signals(run_id, kind, payload)   -- repair{kind}, escalation, breaker_trip,
                                 -- interrupt, error_streak, failed_cmd,
                                 -- gate_fired, digest
bench_trials(run_id, bench_task, trial, verdict)
decisions(operation_id, execution_id, kind, input_hash, policy_hash,
          model_revision, adapter_version, result_json, applied_choice,
          fallback_reason, run_id, recorded_at)
executions(execution_id, task_id, task_generation, lease_generation,
           execution_version, genome_hash, guard_hash, binding_manifest_ref, phase, state,
           attempt, next_attempt_at, deadline_at, cancel_requested)
operations(operation_id, execution_id, task_generation, lease_generation,
           binding_id, kind, inputs_json, state, result_json, started, completed)
execution_events(execution_id, sequence, kind, payload, recorded_at)
dispatch_intents(operation_id, execution_id, payload, enqueued_at)
directives(directive_id, source_ref, version, target_scope, objective,
          acceptance_ref, resource_limits, external_authority, constraint_version,
          recorded_at)
component_bindings(binding_id, component_api, api_version, implementation,
                   implementation_version, config_hash, capabilities,
                   generation, state)
```

Per-run digest fields: files touched, commands run and outcomes, repairs by
kind, escalations, breaker trips, gate nudges, cache-hit ratio, cost, and decision
choices/fallbacks. Decision inputs and receipts live in `operations`; `decisions`
is the queryable projection, committed with the corresponding execution event.

Sources: normalized `HarnessApi` events and usage snapshots through `TelemetryApi`.
The OpenCode adapter privately translates SSE, `tool.execute.after`,
`/api/experimental/session/stats`, and `/api/model` pricing into those records.
The conductor and telemetry service never parse native harness responses.
Specialized adapters report their own usage, latency, and resource measurements;
local inference records zero provider charges separately from resource overhead.
All inference attempts, including failed or shadow decisions, count toward run
economics. Unavailable usage is recorded as unknown rather than zero.

Leaderboard cells: (harness profile, model, role, task family) → success rate,
$/merge, p50/p95 latency, repair rate, escalation rate, cache-hit ratio.
Decider cells use (model revision, decision kind, task family), tracking accuracy,
calibration, fallback rate, routing regret, and downstream outcomes against the
static baseline. Pin local weight revisions and adapter versions in evals; a
changed revision is a new candidate under the onboarding policy.

Degradation detection: compare rolling cells within the same task family only
(never across families), alert on movement past noise. Hard API failures can
auto-revert a route; quality claims only alert and wait for evidence.

---

## 10. Cheap-model stretching

Derived from studying dirge (github.com/dirge-code/dirge) — a Rust agent built
around keeping weaker models on rails. Ideas only; dirge is GPL-3.0, no code
or prompt text is copied.

| Principle | Home in Agalma |
|---|---|
| Mechanical guards over cooperation | Rust gates; nerve counters |
| Tool-input repair | nerve `execute.before` + schema hints; telemetry |
| Escalation on mechanical signals | conductor ladder: repair failure, error streak, verification red |
| Structured interventions (reflect-then-pivot) | conductor: `synthetic` message + optional `interrupt` on repeat loops |
| Failure-streak checkpoint | nerve counters → conductor |
| Phase isolation, minimal handoffs | session model (§6) |
| Memory outside context (two-tier + FTS search) | ledger + skills; `agalma-mcp` search; injected via `context` hook |
| Post-session curation into memory/skills | postmortem agent → genome |
| Token economy (hash edits, minified reads, output relay) | deferred tool overrides; output relay early |
| Prefix-cache accounting | usage events → ledger; cache-hit ratio in KPI |
| Model-aware steering fragments | genome prompt fragments |
| Few-shot exemplars, lexical retrieval | conductor prompt assembly |
| Cheap default, strong on demand | routing table + escalation ladder |
| Bounded decisions with small models | decider interface: routing, triage, memory/context ranking; static fallback |

OpenCode limits to acknowledge: no stream-level stall detection (use timeouts
+ interrupt), no pre-disk syntax gate (validate in `execute.before` or accept
post-write LSP + nudge), hooks cannot do everything. Adoption is data-driven:
measure OpenCode's actual cheap-model failure classes on the bench before
porting a mechanism; build only what the data justifies.

---

## 11. Safety and governance

- macOS MVP sandbox: Seatbelt via `sandbox-exec`, applied to the entire
  harness worker and its descendants through `SandboxApi`. Each attempt has an
  independent checkout and disposable runtime, build, cache, and temporary
  directories. Writes are
  restricted to those directories; the main checkout, ledger, conductor state,
  personal credentials, and protected evaluation files are inaccessible.
  S0b proved the boundary on macOS 26.6.2 (59/59 assertions, fail-closed
  initialization, negligible idle overhead; [results](spikes/s0b-confinement.md)).
  Production conditions: conductor-side fail-closed launch gate, attempt-scoped
  `TMPDIR`/`XDG_*`, per-attempt rendered toolchain read paths, narrowed
  `mach-lookup`, and termination that kills the process group/tree — a leader
  SIGTERM alone orphans descendants, and macOS may reject a group signal
  (signal descendants explicitly; S0d).
- Verification runs separately under a sandbox, with protected tests readable
  but unwritable. Candidate build scripts and tests receive the same confinement.
- Permission rulesets per agent supplement the OS boundary. Integration and
  privileged factory operations run through the conductor; workers receive no
  Git hosting credentials. Provider credentials remain outside the worker and
  are supplied by a narrow authentication proxy. The bridge exposes authorized
  operations, not general access to conductor state.
- Dependency downloads are permitted in the MVP. This does not establish a
  general network isolation guarantee; workers must not receive secrets through
  files, environment variables, or service responses.
- Unattended execution requires successful sandbox initialization. Failure
  parks the task without launching the worker. `sandbox-exec` is deprecated;
  compatibility and the deny matrix are proved in S0b (macOS 26.6.2; re-verify
  on OS upgrades). If confinement cannot be established, a Linux
  container in a VM is required before unattended execution.
- Protected paths: bench, guards, policy files. Denied by rules and checked
  before merge; the factory never edits its own judge.
- The active directive supplies goals, scope, acceptance intent, budgets, and
  authorized external actions. It cannot alter platform constraints, protected
  fixtures, or evaluator integrity. A new human directive can change authorized
  scope; the factory has no per-change approval channel or authority to expand it.
- Budget: per-task soft (interrupt and replan) and hard (kill); daily cap
  (default $10, soft-pause at 80%) → low-cost idle mode, not full stop.
- Kill switch: sentinel/signal; stops scheduling, interrupts sessions;
  terminates process trees and persists a local stop latch checked before every
  dispatch and privileged operation, including recovered jobs. Human-only resume.
- External actions (push/PR/deploy), spending, and target access require authority
  in the active directive. Missing authority means skip, reject, or park and report;
  the factory does not pause completed work for a human approval click.
- Safety warnings and destructive confirmations are always written in full
  English, never compressed.

---

## 12. Stores

| Store | Holds | Authority |
|---|---|---|
| bd (beads) | task definitions, priorities, dependencies, execution status projection | source of truth for *what to do* |
| SQLite ledger | execution/phase state, deadlines, approvals, jobs, decisions, runs, signals, trials, models, digests, operation intents/receipts | source of truth for *execution progress and what happened* |
| git | code, genome, tags, promotions | source of truth for *what was accepted* |
| Harness runtime cache (initially OpenCode DB) | sessions, transcripts | disposable cache |

Consumers access these stores through their owning nano service APIs. bd and
OpenCode IDs, SQL, Git command details, and backend state formats stay private.

---

## 13. Milestones

**S0 — spikes.** Four timeboxed spikes, each with a kill/fallback decision at
exit. S0a and S0b can run in parallel; S0c is independent; S0d starts when
their results are in.

- **S0a — harness.** On the installed OpenCode v2: spawn `serve`, capture
  password, hand-rolled client hits `/api/info`, create session, prompt, read
  the event stream, prepare an independent checkout, run one command inside a
  session. Determine whether `execute.before` can block (or only mutate input).
  Verify `/api/location/reload` and the stats/pricing surface. Pin the exact
  OpenCode version and freeze or correct the endpoint list. Keep native
  HTTP/SSE/configuration inside the OpenCode implementation.
- **S0b — confinement.** Prove Seatbelt confinement for built-in file tools,
  shell descendants, build scripts, shared Git metadata, credentials, and
  protected tests; prove loopback HTTP to the owned server and the nerve Unix
  socket work while arbitrary network is denied; prove sandbox initialization
  fails closed. Validate the external provider authentication proxy and narrow
  conductor bridge. Measure idle memory/CPU and shutdown behavior on macOS.
  Verdict: Seatbelt acceptable, or container-in-VM required before unattended
  execution.
- **S0c — execution.** Embedded dispatch through `ExecutionApi`/`LedgerApi`
  for the minimal serial executor: atomic state and intent recording, duplicate
  delivery handling, durable receipts, and restart reconciliation; crash matrix
  before/after persistence and effect boundaries. Pin SQLite/schema versions
  and define the handoff to the bootstrap project.
- **S0d — seam freeze.** Specify the decision interface and static baselines;
  record optional decider adapter compatibility and resource requirements;
  unsupported inference falls back to static policy and does not block the loop.
  Define the contract tiers, capability profiles, bindings, and state ownership
  ([component contracts](component-contracts.md)). Exercise the harness scenario
  through `HarnessApi` with OpenCode and a deterministic reference adapter by
  changing only the binding. De-risk everything before building.

**M0 — skeleton.** Conductor daemon: supervisor, server lifecycle, ledger,
minimal internal serial executor, explicit SQLite task/phase state, idempotent handlers and
operation receipts, local kill switch, one hardcoded task end-to-end. Wire the
initial nano services through typed Agalma APIs and persist component bindings.
Exit: conductor restart recovers the task; a crash after a Git update but before recording
completion neither duplicates the update nor loses completion; surviving worker
processes are reconciled before redispatch; ledger reflects the run.

**M1 — working loop.** bd queue; builder/verifier phases over real repo tasks;
independent checkouts; Seatbelt workers and separate sandboxed verification;
deterministic verification; merge on green; cost recorded; digest
generated. bd intake uses stable execution IDs, fenced claims, and reconciled
status projections. Routing/triage choices use the versioned decision contract
with static policies and recorded outcomes. Exit: seeded batch processed; success
rate and $/merge measured; duplicate dispatch and expired-lease recovery cannot
merge twice; a staged harness rebind between attempts preserves the conductor
scenario and keeps recovery on the pinned implementation. Each production
replacement requires its own real confinement, accounting, and recovery evidence.

**B0 — durable execution bootstrap (starts after M1).** Seed the
[bootstrap project](durable-execution-bootstrap.md) as factory work. Use the accepted
executor to design, implement, and evaluate the internal Rust/SQLite kernel in
bounded increments: transition/history API, activity dispatch and reconciliation,
durable timers and directive updates, cancellation/fencing, then upgrade compatibility.
Run candidates against isolated state and protected recovery fixtures. Code stays
under transitional oversight until P1. Exit: a gate-passing kernel resumes interrupted tasks and waits,
rejects stale workers, reconciles a crash after merge, preserves pending state
across a compatible upgrade, and meets measured local resource targets. The
accepted runner remains available until the replacement passes its gates.
The bootstrap improves the implementation behind `ExecutionApi`; consumers keep
that contract. Exercise compatible replacement and ledger state export/import.

**B1 — component extraction (starts after B0).** Extract modules from the binary
along the [contract tiers](component-contracts.md), one boundary at a time,
ordered by replacement pressure and crash-containment payoff. Each extraction
keeps the Agalma API and consumer code unchanged; the architecture test forbids
cross-module private access before and after. Old and new bindings run in shadow
on the same scenario and fixture packs; activation happens between attempts and
retains the prior binding and its state recovery path for rollback. Exit: the
component map is realized where replacement pressure exists; every extraction
passed dual-binding evidence, staged rebind, and rollback without state loss.

**M2 — bench + promotion + nerve.** Bench (fixtures + history replay),
genome versioning, paired evaluation, auto-promotion of genome wins. Nerve
core: repair, context injection, permission bridge, compaction summarizer.
Model onboarding: screen + full eval + routing mutation. Memory/skills
curation postmortem. Evaluate decider candidates for routing and triage in shadow
mode, then activate only if they pass decision fixtures, fallback/recovery gates,
and downstream quality/economics comparisons. Memory/context and task ranking
follow as separate evaluated decision kinds. Exit: factory improves its own
prompt/skill unattended, keeps it, and the ledger shows the win.

**M3 — always-on baseline.** Scheduler, budgets, recon/mining, digest cadence, recovery
drill; conductor supervision, SQLite backups/restore, retention, and schema upgrade
procedure. Exit: 72h unattended; budget respected; clean kill/resume;
degradation alerts working; restore drill reconciles execution state, receipts, bd, and
Git before resuming dispatch; component drain/rebind, rollback, and recovery with
an old pinned binding work through the declared APIs.

MVP is complete at M3: self-improving, always-running, and bounded. The P1
gate then removes transitional human code oversight for the no-review destination.

**P1 — autonomy gate.** Before removing transitional human code oversight, prove
all of the following: protected acceptance/evaluator fixtures; reproducible
verification against immutable candidate SHAs; paired bench and regression veto;
directive/constraint enforcement; budget enforcement for failed, local, and remote
inference; durable crash/lease/cancellation reconciliation; automatic rollback of
bad promotions; and directive-bounded target actions. Run shadow and canary changes
and demonstrate self-correction over the S0/M3 recovery and workload suites. When
all gates pass, the policy automatically enables code and kernel integration under
the active directive. If a gate later regresses, disable promotion, revert the
affected change, and return to the last accepted version without requesting human
review.

---

## 14. Deferred

- Further hardening after the P1 autonomy gate: parallelism (N builders,
  serialized merge queue), container sandbox, dashboards.
- Temporal: deferred while Agalma is a local tool; reconsider for distributed
  execution. Self-hosted deployment and PostgreSQL are outside the local MVP.
- General workflow platform: distributed workers, arbitrary async-function
  replay, Temporal protocol compatibility, and an independently shipped execution
  service. The bootstrap kernel is an internal factory component.
- P2 generalization: any repository, external intake (GitHub issues), deployer
  role.
- P3 evolution: populations of genomes, meta-evaluation, crossover.
- P4 OpenCode maximization: factory targets OpenCode itself; deeper
  provider/VCS/PTY/RPC use; dirge cross-bench as a reference bar; tool
  overrides (hash-anchored edit, minified read) as genome experiments.

---

## 15. Decisions log

| Decision | Choice |
|---|---|
| Component architecture | Siloed nano services; every component replaceable through a versioned Agalma API |
| Local deployment | Typed Rust modules and supervised processes; shared domain contracts |
| Harness | `HarnessApi`; initial adapter runs OpenCode v2 as `opencode serve` |
| Component replacement | Compatible bindings; drain/reconcile, preserve durable state, pin in-flight operations |
| Contract tiers | Tier 1 seams frozen at MVP; Tier 2 schemas and ownership now, replacement ceremony when scheduled |
| Component extraction | Bootstrap project B1 after B0; unchanged APIs, dual-binding shadow, staged rebind, oversight until P1 |
| S0 structure | Four timeboxed spikes: harness, confinement, execution, seam freeze |
| Agalma language | Rust |
| Durable orchestration | Own native Rust/SQLite kernel; Temporal concepts as reference |
| Bootstrap | Minimal serial executor first; factory develops kernel after M1 |
| Execution dependencies | Internal dispatch; Apalis remains research only |
| Temporal | Deferred; revisit for distributed execution |
| Recovery | Durable phase checkpoints + operation receipts + fenced claims |
| Integration | Agalma domain APIs; OpenCode HTTP/SSE client private to its adapter |
| Inner-loop bridge | Canonical events/policy requests; OpenCode nerve is an adapter detail |
| Tools for agents | `FactoryToolsApi`; initial Rust MCP gateway |
| Queue | `TaskQueueApi`; initial bd adapter |
| Promotion | Autonomous, evidence-gated genome/code/kernel integration after P1 |
| Human role | Versioned directive supplies goals, constraints, budgets, and authority; no per-change review |
| Platform constraints | Enforced by the conductor; cannot be weakened by a directive candidate or factory change |
| Directive authority | New directives may change authorized goals/scope; work outside scope is parked and reported |
| Self-correction | Bounded repair, rerun pinned criteria, reject/revert on failure, continue on other tasks |
| Sessions | Fresh per (task, phase, attempt) |
| Topology | Persistent composition/supervision; disposable sandboxed harness runtime per attempt |
| MVP sandbox | macOS Seatbelt via `sandbox-exec`; fail closed; prove in S0 |
| Attempt isolation | Independent checkout and Git metadata per attempt |
| Credentials | External provider authentication proxy; no worker Git credentials |
| Network stance | Dependency downloads permitted; no general network isolation guarantee |
| KPI | Cost per merged change at fixed success bar |
| Model evals | Bench once per tuple; production telemetry as canary; triggers only |
| Decider models | Optional choice/scoring interface with interchangeable models; Strands is a reference |
| Decision authority | Rust filters eligible actions and enforces guards; recorded model recommendations |
| Decision rollout | Static baseline → shadow evaluation → gated active routing/triage |
| Decider runtime | `DecisionApi`; generative inference via `HarnessApi`, optional specialized adapter loaded on demand |
| Arrival evals | Auto-screen/evaluate within directive budget; skip candidates that exceed it |
| Bench v0 | 10 tasks × 2 trials; screen 3 × 3 |
| Budget cap | $10/day default, soft-pause at 80% |
| Goal metric | $/merged change; regression veto |
