# Durable execution bootstrap project

Status: planned. Starts once M1 can process real repository tasks with the
accepted minimal executor. The factory's first infrastructure bootstrap project
is to develop its own durable execution kernel in native Rust with local SQLite.

## Objective and boundary

Make Agalma's phase progression, retries, deadlines, directive updates, and recovery
an explicit nano service behind `ExecutionApi`. Follow the
[component contracts](component-contracts.md): consumers use the versioned API,
and the kernel accesses persistence only through atomic `LedgerApi` operations.
Policy, sandbox lifecycle, and integration remain separate API responsibilities.
The kernel persists scheduling decisions and outcomes and invokes registered
activity handlers through their component contracts.

The design takes inspiration from Temporal's [event history](https://docs.temporal.io/encyclopedia/event-history),
[deterministic workflows](https://docs.temporal.io/workflow-definition), and
[idempotent activities](https://docs.temporal.io/activity-definition). Its local
scope uses explicit state transitions and named steps. It does not implement
arbitrary async-function replay, Temporal's APIs/protocol, distributed execution,
or an independently deployed service.

## Bootstrap sequence

| Stage | Delivered by | Required behavior |
|---|---|---|
| Seed executor, M0–M1 | Initial implementation | Fixed factory phases through `ExecutionApi`, atomic SQLite persistence through `LedgerApi`, operation receipts, exclusive conductor lock, fenced claims, restart reconciliation, and local kill switch. Enough to safely run real tasks. |
| Kernel bootstrap, B0 | Factory-generated code tasks | Extract and extend that executor through bounded changes, running each candidate against isolated state. Temporary startup oversight ends at P1, when the autonomous promotion gates pass. |
| Kernel activation | Accepted conductor | Stop dispatch, settle or reconcile active processes, activate a gate-passing implementation at a controlled restart, check state/schema compatibility, then resume. |
| Component extraction, B1 | Factory-generated code tasks, after the kernel passes its gates | Move modules behind process or socket boundaries per the contract tiers, one boundary at a time, keeping every Agalma API unchanged; dual-binding shadow runs before activation. |

The seed executor remains responsible for running this project until an approved
replacement is ready. Candidates do not execute against the live ledger or replace
the running factory during evaluation. Transitional code oversight can continue
until P1; after P1, code and kernel changes integrate automatically when they pass
the directive and promotion gates. The factory cannot change protected platform
constraints; humans update goals or permitted scope through a new directive.

## Incremental work

1. **Execution model and history.** Define versioned execution states, commands,
   and ordered events behind `ExecutionApi`. Implement a deterministic transition
   function over recorded inputs. Store each transition, event, and dispatch
   intent in one atomic `LedgerApi` commit; database access stays in its owner.
   Keep large artifacts outside history with immutable references. Recovery can
   reconstruct state without executing tools or making model calls.
2. **Activity protocol.** Register named handlers with execute and reconcile
   behavior. Assign stable operation IDs and input hashes. Persist results before
   advancing the phase; return stored results for completed operations. Pending
   operations reconcile external state before retry. New model attempts are
   explicit budgeted decisions, not implicit delivery retries.
   Decider inference uses the same protocol: persist the request and validated
   choice or fallback before dispatching dependent work. Completed decisions are
   reused during recovery; model calls stay outside the transition function.
   Operations pin component/API binding versions; retries reconcile with that
   binding rather than silently switching to the currently configured adapter.
3. **Durable waiting.** Persist retry schedules, deadlines, and incoming directive
   updates. Recover overdue timers after restart without duplicate phase
   advancement. Directive updates have unique IDs and bind to source and
   directive version. Duplicate updates are harmless; a newer directive can
   cancel or replace queued work. Waiting releases agent sessions and workers.
4. **Cancellation and ownership.** Persist cancellation requests; fence stale
   lease generations; integrate explicit process-tree termination. Reclaiming a
   job requires reconciliation of prior processes. The local kill switch blocks
   recovered dispatch as well as new work.
5. **Upgrade and integration.** Version the kernel's persisted contract. Check
   candidates against saved histories and fixture databases; reconstructing state
   may use reducers but never perform external effects. Add explicit migrations
   where needed. Integrate incrementally into the factory and document compatible
   restart, rollback, backup, and restore procedures.
   Preserve the consumer API and define compatible state export/import so another
   compliant execution implementation can replace the kernel.

An event journal and current-state projection must agree. For each transaction,
record a monotonic sequence and update the projection atomically. Reject an input
that attempts to reuse an operation or signal ID with different contents. Treat
unknown execution versions or ambiguous external effects as parked work.

This protocol guarantees durable local decisions and safe reconciliation. It does
not claim exactly-once external side effects: a successful Git or provider action
may outlive the handler's completion record. Adapters must supply evidence of the
effect or park the operation when its outcome cannot be established.

## Acceptance and evaluation

Use an immutable recovery fixture pack, authored as part of the initial factory
directive and protected from candidate edits. Candidates cannot change the tests
used to measure their own recovery behavior.
Activities in the pack use fake effects or disposable repositories and databases.
Inject crashes before and after persistence and external-effect boundaries:

- Crash after transition commit but before dispatch: pending work is recovered.
- Duplicate delivery after activity completion: the recorded result is returned
  without repeating the effect.
- Restart after a decider result or fallback is committed: dependent work uses
  that recorded decision, rechecking eligibility without another inference call.
  Late inference cannot overwrite an applied fallback; changed inputs require a
  new operation ID.
- Git merge succeeds before completion is recorded: recovery recognizes the
  accepted commit and completes ledger/bd updates without another merge.
- A worker survives conductor failure: recovery confirms termination or parks;
  reports from its stale lease cannot advance the task.
- Restart during a retry delay or directive update: the deadline/version survives
  and is applied once; cancellation and the kill latch survive restart too.
- Upgrade with pending executions: compatible state resumes; incompatible state
  parks until an approved migration is available.
- Replace a compatible kernel or activity adapter: canonical IDs, receipts, and
  pending work survive. Existing operations remain pinned to their old binding
  until completed or reconciled; consumer code uses the same Agalma APIs.
- Rollback or restore: the runner checks database compatibility and reconciles
  actual Git/bd state before dispatch.

Compare resource measurements with the seed executor: idle memory/CPU, restart
time, dispatch latency, and database growth. Establish numeric regression limits
from the measured seed before accepting a candidate; keep correctness as a veto.
Record kernel version and execution-definition version with results. All required
code and promotion gates apply; if a required gate is not available yet, the
implementation remains a candidate until it can pass that gate. Bootstrap cannot
bypass required gates or modify the criteria used to judge its own candidate.

The project's output is a gate-passing internal kernel, integration changes,
documented recovery contracts, and measured evidence. It is not complete merely
because an event log or a task queue has been implemented.

## Follow-on bootstrap project: component extraction (B1)

Starts once the kernel passes its gates. Extract components from the modular
monolith along the [contract tiers](component-contracts.md), one boundary at a
time, ordered by replacement pressure and crash-containment payoff. The factory
implements each extraction under transitional oversight until P1.

Per extraction:

- The component map and tier table are the spec; the work moves a module behind
  a process or socket boundary without changing its Agalma API or consumer code.
- The architecture test forbids cross-module private access before and after;
  extraction changes transport, not dependencies.
- Old and new bindings run in shadow on the same conductor scenario and fixture
  packs before activation. Activation happens between attempts; the prior binding
  and its state recovery path are retained for rollback.
- Durable-state components use the versioned export/import contract and preserve
  IDs, receipts, and pending work. Operations stay pinned to the binding they
  started under.

Acceptance: API-identical behavior, dual-binding scenario parity, staged rebind
between attempts, and rollback without state loss. An extraction is complete
when replacement through the declared API is proven, not when a process was
split.
