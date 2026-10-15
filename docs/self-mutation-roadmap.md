# Remaining self-mutation roadmap, revision 1

Date: 2026-10-12. This follows the first SF -> HP -> SM standup and the exact
contracts in `self-mutation-contracts.md`. It turns the remaining planning areas
into bounded implementation tranches. Future implementation remains staged; the
runtime design gate rechecks these plans against actual SF/HP/SM evidence before
release. Planned test paths below do not claim existing tests or passing results.

## Sequence and milestone accounting

1. Finish the shared design gate and first reliable self-build/plugin/measured-win
   tranche, including SM.9's directive-authorized prompt/skill release and SF.10's
   fresh full pipeline proof and SF.11's prerequisite-aware review admission.
   Bootstrap code retains transitional human oversight.
2. Migrate existing production services onto the common platform. Build the
   remaining M2 capabilities as independently scoped plugins/adapters.
3. M3: operate the bounded factory unattended and prove 72-hour recovery/restore.
4. P1: prove controlled replacement and code self-correction, then enable an
   explicitly versioned code-autonomy policy.

M2's first measured-mutation tranche is not the whole milestone. The extensions tranche
is part of the original nerve/onboarding/memory/decider obligations. The M3 exit
depends on both measured mutation and those extensions, plus production service
convergence. P1 depends on M3. Reporting visibility never substitutes for an
admitted workflow capability or production dispatch evidence.

## M2 extensions: nerve, model onboarding, memory and deciders

Keep vendor hooks in the harness adapter. A nerve bridge emits canonical events
and bounded requests; it cannot own durable state, authorization or spending.
Prompt/skill curation and new model/decider activation use the existing SM gate.

| Key | Owns | Behavioral acceptance |
|---|---|---|
| MX.1 | Canonical nerve event/policy request envelope; adapter profile/version pins | Captured hook fixtures normalize once; unsupported profile refuses effects; native payloads do not escape contract |
| MX.2 | Context injection and compaction summaries from immutable artifact refs | Summary preserves directive, constraints, pins, unresolved operations and evidence provenance; stale/forged summaries cannot change acceptance |
| MX.3 | Bounded tool repair and permission bridge | Invalid calls get bounded feedback; exhausted repair stops; model requests cannot widen grants or sandbox; stop/cancel still denies execution |
| MX.4 | Postmortem memory/skill proposals, rejection memory and retrieval | Dedup and provenance survive restart; retrieval is scoped; curated skill needs full evaluation; stored text cannot authorize actions |
| MX.5 | Model discovery/onboarding candidate registry | New tuple/profile creates one immutable candidate; absent reviewed billing/limits cannot become eligible; changed harness profile needs new evidence |
| MX.6 | Screen -> full bench -> routing candidate promotion | Screen cannot activate; complete downstream quality and economics evidence gates route mutation; quota/cash/resource caps and rollback remain enforced |
| MX.7 | Decision-kind shadow evaluation and bounded fallback | Routing/triage kinds qualify separately; missing/invalid outputs fall back to eligible deterministic policy; local CPU/RAM/time cost is included |
| MX.8 | Integrated M2 extension evidence and milestone audit | Nerve, permission denial, scoped memory, onboarding rejection/win and shadow-to-canary decider proof; no M2 closure from first SM win alone |

MX.1-MX.3 extend the declared HarnessApi profile, not private hub knowledge of
OpenCode plugin internals. MX.5 starts from existing Router registry/catalog work;
it does not reimplement SR.1-SR.4. MX.6 consumes SM qualification/promotion and
Router public contracts. Provider catalog changes are candidates for review,
not automatic pricing facts. Model tuple includes provider/model/variant and the
evaluated harness capability/configuration profile. Rebench on a new tuple,
version bump, directive change or degradation trigger, not periodically without
a reason. MX.7 never hides local inference overhead as zero cost.

## Production service convergence

Common platform code is sufficient for unfamiliar plugin installation, but the
existing services must individually adopt it. Migration changes one seam at a
time and retains the previous binding for reconciliation. A private binding may
exist inside a hub-owned typed adapter during migration; it is declared as a
deviation and cannot become the wiring recipe for new plugins.

| Key | Owns | Behavioral acceptance |
|---|---|---|
| CV.1 | Real `work` driver behind the selected ExecutionApi with caller-side effect registry | Seed/kernel/process parity on immutable packages; restarted task dispatches through recorded handler/binding; no skipped replay or seed-only fallback |
| CV.2 | Auxiliary ledger reads/writes and standalone CLI routed through selected LedgerApi | Digest, audit, routing, status/economics/control use public adapters; recording process ledger sees the calls; SQL exists only in ledger implementation |
| CV.3 | Ledger and execution plugin packaging/lifecycle | Discover built-in packages through manifests; compatible state export/import and old-generation reconciliation; recovery bootstrap can start the ledger before reading other durable registry state |
| CV.4 | Workspace and taskqueue plugin packaging/routing | Scope-aware workspace calls and queue claim/projection cross hub-owned adapters; no spoke socket discovery or direct bd/storage bypass |
| CV.5 | Sandbox and harness plugin packaging/routing | Parent credentials, proxy scope and worker termination survive common supervisor lifecycle; old execution pins are honoured; READY/callback timeout paths bounded |
| CV.6 | Router integration on common plugin platform | Consume SR.11/SR.12 delivery; generic install/report/action paths; ineligible/unknown/failed routes never bypass hard admission gates |
| CV.7 | Generic migration and rollback scenarios | Drain and replacement with pending work; unknown old generation stays unknown; compatible rollback preserves component and execution receipts |
| CV.8 | Production convergence evidence and architecture audit | Real conductor/CLI use public bound APIs; install new service without recompiling consumers; deviations enumerated with explicit exit obligations |

Bootstrap ownership: the external launcher has the installed ledger package pin
and state path as minimal static bootstrap configuration. It starts and validates
that ledger, then reconstructs the hub registry via its API. This avoids a circular
requirement to read a registry from a ledger that has not started. The bootstrap
configuration is versioned and protected from ordinary plugin mutation.

CV.1 depends on SF recovery/cancellation and HP's protocol proof. CV.2 revisits
the accepted B1-F2 SQLite deviation; it does not rewrite its old evidence as if
the earlier scope had failed. CV.6 consumes existing Router tickets rather than
creating a second router. Evidence distinguishes a manifest entry from actual
production method dispatch.

## M3: always-on operations

The hub owns scheduling and authority. Scheduler policy is canonical immutable
data; observation/mining plugins can propose tasks but cannot silently dispatch
them. A single durable operator lease owns admissions. Serial dispatch remains
the baseline; no hedged calls, parallel builders or distributed scheduler.

Initial cadence: ready-work scan every 5 seconds when idle; reconciliation every
30 seconds; mining every 15 minutes; digest every 24 hours. Persist the next due
time and last receipt for each job. Coalesce missed ticks after restart into one
reconciliation, not a burst of historical work. Empty/blocked queues sleep until
the next tick or state notification. Failures use capped exponential backoff.

Extend the SM.9 release service instead of creating another approval mechanism.
Release policy checks active directive, target/class allowlist, decomposition
approval, complete immutable inputs, closed blockers, scope/protected constraints,
role eligibility and remaining directive/task budgets. It records the exact
decision and reason before changing the queue projection. Projection is
reconcilable; a cleared defer date, a validator `decomp-approved` label, or a
resolved bootstrap human gate alone is not trusted automation authority.
Bootstrap Rust and arbitrary executable plugins remain under their existing gates
(the native human execution-approval gate for staged children).

| Key | Owns | Behavioral acceptance |
|---|---|---|
| M3.1 | Versioned operator directive, cadence and automatic-release policy | Fixed input gives fixed allow/deny receipt; cancelled/unknown policy suppresses effects; proposals cannot approve themselves |
| M3.2 | Durable scheduler, single-owner lease, bounded admission and idle/backoff | Injected-clock scenarios prove one job per due tick, no spin/burst, duplicate-owner fencing and restart continuity |
| M3.3 | Aggregate directive/day cash, quota and local resource governor | All roles, benchmarks, canaries and retries reserve against hierarchical caps atomically; reset/restart cannot erase liabilities |
| M3.4 | Reconciliation/mining/digest jobs and generic alert reports | Duplicate ticks do not duplicate tasks/digests; degraded/unavailable state reaches UI; outputs never auto-authorize work |
| M3.5 | External supervision and clean lifecycle | Versioned launchd template plus portable process fixture; restart obeys pause/cancel/lease; no two writer/admission owners |
| M3.6 | Backup/export, artifact retention and disk-pressure behavior | Complete backup includes ledger/component state, bd projection provenance, Git refs and referenced objects; pins protect objects from collection |
| M3.7 | Restore and schema-upgrade reconciliation procedure | Restored isolated state checks external Git/bd/effects before new work; stale generation/unknown receipts park; migration is quiescent and transactional |
| M3.8 | Crash/kill/resume/degradation drills | Fault matrix at admission, provider send, merge, component action and promotion preserves receipts/budgets/pins and stop/cancel semantics |
| M3.9 | Genuine 72-hour unattended evidence and MVP audit | Wall-clock duration, admitted workload, total resource usage, alerts and restore drill recorded; accelerated fake clocks do not claim the 72-hour exit |

Aggregate caps use a hierarchy: platform -> active directive -> task/evaluation
scope -> provider request. A reservation consumes all applicable caps in one
admission decision; no race between unrelated scopes. Cash windows track incurred
charges and outstanding liability across resets; quota windows use reviewed
provider observations. Unknown usage does not disappear at midnight. Local
inference records CPU time, memory ceiling and wall deadline even with zero cash.

Backups are hub-triggered through public APIs, with a consistency watermark and
artifact inventory. A restore stops dispatch, uses an isolated destination,
checks receipt/external-effect agreement and increments ownership fencing before
resume. Backups cannot establish that already emitted external effects vanished.
Retention collects only objects unreachable from active/pinned packages,
operations, candidates, rollback refs and retained backups. Low disk headroom
suppresses new builds; it does not delete pinned evidence to keep running.

Live 72-hour operation needs an explicit directive with nonzero total caps and
workload. Planning does not invent a dollar allowance or start that run. The
deterministic drill can pass earlier, but M3 stays open until genuine operations
evidence and a restore drill satisfy its original exit.

## P1: core update and removal of transitional code oversight

The running candidate cannot be its sole evaluator, launcher, or rollback owner.
Use a small external supervisor/bootstrap process with a stable versioned
handover API, its own bounded health watchdog, and retained accepted packages.
The supervisor is outside the first autonomous code-mutation scope. Changing
that recovery root requires a separately versioned platform change.

An immutable `CoreCandidate` pins source SHA, executable/package digest, build
recipe/toolchain/dependency lock, configuration, compatible contract and ledger
schemas, protected suite/evaluator evidence, current accepted baseline and
directive. Reproducibility means either identical binary digests or a documented
deterministic normalization with exact residual differences; compile success is
not the claim. Build and verify in isolated state before touching the live factory.

| Key | Owns | Behavioral acceptance |
|---|---|---|
| P1.1 | Immutable core candidate and compatible update manifest | Candidate cannot change protected suites, grants or budget limits; incompatible binary/config/schema refuses staging |
| P1.2 | Independent reproducible build and protected S0/M3 evaluation | Immutable SHA/oracle/toolchain recorded; candidate-authored pass markers ignored; regression veto and complete accounting |
| P1.3 | External launcher/supervisor and handover journal | Supervisor survives hub crash; writer/admission lease transfer is fenced; duplicate update IDs reconcile without starting two owners |
| P1.4 | Drain/checkpoint/stage/activate update transaction | Existing pins settle or remain on compatible old generation; activation never assumes unknown effect completion; crash boundaries reconcile |
| P1.5 | Failed-start and health-regression rollback | Bad binary/handshake/probe triggers prior compatible runner; durable receipt; live DB never rewound with binary |
| P1.6 | Shadow core-candidate evaluation and bounded canary | Candidate runs on isolated snapshot with target effects denied; paired outputs and resource targets checked before limited activation |
| P1.7 | Explicit directive-bounded code autonomy policy | Only a passing P1 evidence revision enables automatic core integration; new directive cannot widen protected platform authority |
| P1.8 | Independent P1 evidence and code self-correction proof | Real retained code improvement, rejected candidate, crash during handover and automatic rollback; all original autonomy requirements audited |

State compatibility and binary compatibility are separate. A binary rollback
requires a writer compatible with the current ledger schema; a pre-upgrade DB
restore is an operations recovery with external-effect reconciliation. Initially,
automatic core updates are schema-preserving. A schema-changing candidate parks
for the separately authorized migration procedure. This keeps a first core
self-update from claiming automatic live database downgrade.

Shadow processes have read-only isolated state and no target-action credentials.
Canaries begin only after M3 and the complete protected evaluation. The external
supervisor owns activation/rollback operation IDs; the hub can request but cannot
forge its receipt. Failed startup denies new effects before returning to the
accepted runner. A paused factory remains paused across either direction.

P1's destination is automatic routine code integration under an active directive,
not a permanent approval queue. Enable it only after the original protected
acceptance, evaluator, paired bench, scope, budget, crash/lease/cancel, rollback
and directive-bounded action requirements all have recorded evidence.

## Backlog reconciliation and Router policy

Keep `agalma-i5h.7` as evidence of authored/validated/staged decomposition. Its
record does not prove approval -> execution -> final review -> epic closure.
Use a fresh bounded standup feature for that full proof, not duplicate TUI work.

The `agalma-63m` audit is recorded in `tui-pilot-reconciliation.md`. Its ten
children duplicate completed `agalma-3kr` behavior and are retired with individual
evidence references. Missing durable cancellation, registry-driven content,
public ledger convergence and fresh pipeline execution have separate SF.4,
HP.6, CV.2 and SF.10 tasks. The pilot is not live factory execution evidence.

Router normalization is precise:

- A fallback is another candidate evaluated by the same hard gates.
- No eligible candidate, unavailable Router, unknown budget/policy, or missing
  reviewed billing record parks; it does not invoke an unchecked v1 table.
- An explicit model override is recorded but still passes all hard gates.
- A statistical quota reserve can guide ranking; provider request admission still
  applies the reviewed worst-case liability before any billable request.
- The Router's process package uses common discovery/supervision/reporting, not a
  bespoke environment-selector/supervisor/UI path.

Apply these amendments to existing SR.5/SR.11/SR.12 task context. The original
Router design may continue to describe legacy deployment/report-only budgeting;
these accepted architecture and standup contracts govern new production work.

## Implementation handoff

Each implementation child owns one row above, its public surface and a named
behavioral integration test. Use the CP headings, rich notes, factory intake,
explicit dependency edges, structural difficulty, the `staged` label, and a
native human execution-approval gate. Cross-epic
dependencies must be real bd edges even when MACHINE consumes resolves only
sibling symbols. The runtime design gate must validate the actual predecessor
contracts and these resource/default values before those later children release.

Use existing locked dependencies and bound APIs. Preserve concurrent edits.
Sessions do not commit, rewrite acceptance/oracles, close dependencies or claim a
milestone from isolated tests. Evidence children report exact bindings and remaining
deviations, and distinguish scripted mechanics, live inference, and real elapsed
operations. No paid run, service launch or task release occurs in this planning.
