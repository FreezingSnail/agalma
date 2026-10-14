# Self-mutation standup: reliable builders, hub plugins, measured promotion

Status: implementation handoff, contract revision 1, 2026-10-12. The design gate recorded
in `docs/self-mutation-standup-beads.md` must close before implementation children
are released. This document is the shared context for those children; each bead
owns its bounded implementation and behavioral tests. No implementation or paid
run is performed by filing this plan. Exact decisions, schemas, default policy,
failure semantics and ownership amendments are in
[self-mutation-contracts.md](self-mutation-contracts.md); they take precedence
over the original proposed field lists below. The user selected prompts/skills
first and higher task success with regression/spending bounds. Remaining M2,
production convergence, M3 and P1 work is in
[self-mutation-roadmap.md](self-mutation-roadmap.md).

## Goal and sequence

Build a factory that can implement a bounded change to itself, expose new modules
as plugins in its existing UI, and retain a prompt or plugin improvement because
independent, recorded evaluation demonstrates a win. Preserve the hub-and-spoke
rules in `AGENTS.md`.

Sequence:

1. Freeze contracts and failure semantics at the design gate.
2. SF: reliable self-build execution, complete task delivery, scope and cost
   admission, cancellation and recovery, builder-only model selection.
3. HP: common plugin protocol, discovery, supervised registry, hub routing,
   reports, dynamic UI, actions, and independent plugin evidence.
4. SM: immutable genome candidates, protected paired evaluation, proposal tasks,
   measured promotion, canary rollback, and a complete first self-mutation.

SF and HP are new epics. SM is the first implementation tranche under the
existing M2 epic `agalma-cg9`. This tranche does not silently declare the remaining
M2 nerve, model-onboarding, memory, or P1 autonomy requirements complete.

The first automatic mutation domain is prompts/skills. Explicitly allowlisted
plugin packages follow as a subsequent rollout. Hub, kernel, sandbox policy,
credential authority, protected
fixtures, and hard limits remain outside automatic mutation authority until the
existing `agalma-52v` autonomy gate is satisfied. Bootstrap Rust work uses the
existing transitional code oversight. Automatic UI appearance alone never grants
permission to schedule tasks, launch builds, or activate code.

## Current code and known hazards

- `prompts/builder.md` still describes the M0 one-line broken-function fixture.
  `phases::render_builder_prompt` appends a title and acceptance commands, not the
  issue's complete description/design. An elaborate bead is ineffective if that
  context never reaches the builder.
- `Task` carries parsed block fields but not the complete issue description or
  design. `work::recover_inflight` reads the current task from bd and rebuilds
  activities, so edited acceptance can change an in-flight execution.
- The work recovery path drives `Executor::step` without invoking the history
  and projection agreement check. Calling the seed executor's global `recover`
  with one task's activities is also wrong: it can probe another task's intents
  through the wrong activity set. Add an execution-scoped gate/reconciliation.
- Seed `Executor::cancel` records a request and sets the global kill latch. It
  does not durably settle the selected execution. Resume clears the latch.
- `FileStopControl` treats ledger read errors as not stopped. Planning sessions
  are outside the seed executor's ordinary admission path.
- Acceptance runs in a fresh clone of the candidate, which is insufficient to
  protect an oracle that the candidate itself changed. Protect the oracle and
  verify immutable candidate identity separately from checking out clean files.
- Task budgets are display-only. Unknown model usage must not become zero spend;
  stopping a session cannot undo already incurred provider charges.
- Global `--model` overrides multiple roles. Add an independent builder override
  so Flash selection does not also select the designer or reviewer model.
- `components.rs` enumerates six component types; `watch.rs` has fixed panes.
  Build registry-driven component/detail contents while retaining the useful
  execution/economics/timeline views.
- B1-F1 (`agalma-5de`) delivered the caller-side handler protocol, but `work`
  still binds the seed executor. Do not describe the kernel as production-bound
  merely because isolated protocol tests pass.
- B1-F2 (`agalma-1o3`) accepted auxiliary SQLite access. Connect multiplexes
  connections now, so that framed-server deadlock rationale no longer justifies
  introducing new direct accesses. New plugin/mutation modules take bound APIs.

Read `docs/planning-pipeline.md`, `docs/component-contracts.md`,
`docs/b1-execution-evidence.md`, `docs/b1-ledger-evidence.md`, and the existing
tests before changing the corresponding seam. Existing `agalma-3ej`, `agalma-ud1`,
`agalma-fez`, `agalma-9m2`, and `agalma-cmq` remain their own tickets. The staged
`agalma-63m` TUI pilot overlaps completed TUI work; do not reimplement it here.

## Design gate deliverables

Review and freeze the proposed schemas below, their explicit error variants,
storage ownership, migration procedure, authorization profiles, compatibility
rules, resource bounds, and fixture expectations. Check each child for correct
file ownership and real dependency edges; revise the affected bead and this
document together if a contract changes. The gate produces a design receipt
with the document revision and schema/example hashes. It is architecture work,
not a Flash builder task or a paid live-run authorization.

Use existing pinned dependencies and Connect envelope transport. Do not assume
new downloaded crates, dynamic Rust ABI loading, a second database, a browser
UI, or a replacement of the entire execution engine. Resolve uncertain provider
reservation bounds and current ledger migration constraints before releasing
the affected children; do not leave Flash to invent these contracts.
Also settle callback/reentrancy behavior: the existing generic Connect dispatcher
holds a shared server mutex during synchronous handler execution. A hub request
that waits on a spoke while that spoke calls back into the same locked hub can
deadlock. The common host must release shared state locks across external RPCs
or use an explicitly specified asynchronous operation protocol, with a bounded
callback conformance test.

## SF contract: immutable task package and execution admission

Canonical `TaskPackage/v1` (exact schema in the revision-1 contracts):

- task/execution IDs, generation, source repository identity and resolved base
  commit, directive ID/version;
- complete issue description, design, notes, machine block, acceptance strings,
  parent epic/design references, relevant closed-dependency handoffs, and the
  shared design/repository instruction content with source path and digest;
- permitted file paths, protected paths, immutable verification input refs;
- max attempts, cash allowance, usage-unknown policy, role/model selections and
  binding pins;
- package schema version, canonical content hash, recorded timestamp.

Read the source definition once for a new execution; persist the package before
announcing its first dispatch. Any staged artifact must be durable before its
ledger reference commits. Recovery resolves that reference and validates hashes;
it never substitutes current bd content, prompts, rules, routes, or acceptance.
Missing, incompatible, or damaged packages park with a reason. Existing in-flight
records without sufficient immutable inputs park explicitly; do not manufacture
historical inputs. Terminal historical records remain inspectable.

Canonical integrity hashing uses the revision-1 SHA-256 utility and encoding.
The kernel's existing FNV helper remains a legacy conflict detector, not an
artifact integrity guarantee. SF.2 owns the pure public helper and direct use of
the already locked ring dependency; no new package version/download is allowed.

Render general repository instructions and all task sections, including relevant
design, notes, dependency handoffs, and retry artifacts. Large context uses
immutable readable artifact refs, never silent truncation or a current bd fetch.
Do not expose held-out oracle answers through these context artifacts. Keep the
fixture command on a separate prompt path.
`--builder-model` affects only the builder; preserve documented legacy `--model`
semantics and existing planner/reviewer eligibility. Route receipts pin the
actual per-role selection so restart cannot silently adopt a new model.

Recovery is execution-scoped: validate history/projection before probing any
effect, reconcile the pinned task's survivors, then allow its next dispatch.
Cancellation is durable per execution and distinct from global pause. Persist
intent before cleanup, record termination evidence, settle outstanding work, and
reject later advancement. Duplicate cancel is harmless; resume never reverses
cancellation. Revision 1 freezes settlement states and the compatible
Failed/Parked/Cancelled public projection; both adapters must follow it.

Protected scope is checked before verification and integration, including
renames, deletions, executable-bit changes, symlink escapes, and generated files.
The benchmark/oracle comes from a pinned protected baseline with a read-only
mount; the candidate's pristine clone is the subject, not the judge. Reports
identify candidate SHA and oracle hash.

Cash admission uses recorded spend plus conservative outstanding reservations.
Subscription quota remains a separate quantity. A paid request is admitted only
if its reviewed upper-bound reservation fits available allowance; uncertain
prices/bounds or unknown usage suppress further paid work. Attribute every
generation, attempt, and role; settle or conservatively retain reservations after
timeout/crash. Specify what can overshoot during an already admitted request and
never claim that cancellation refunds cash. Reuse the parent provider proxy for
request admission and keep credentials outside the worker. Deterministic fake
provider tests are the default acceptance; live proof is a separately gated run.

## HP contract: independently installed plugins

Canonical JSON `PluginManifest/v1` contains `plugin_id`, package version/digest,
plugin protocol major, offered domain APIs and versions, declared capabilities,
required hub services, executable path and argument vector, configuration schema,
default configuration reference, report schemas, declarative views, and action
descriptors. PluginInstallation separately pins actual configuration and grants.
Use a configured plugin directory. Resolve package-relative paths canonically;
refuse duplicate IDs, unsupported majors, escaped entrypoints, cyclic required
dependencies, and ungranted capability requests. Do not execute shell strings.

Generic `PluginApi/v1` operations are Describe, Health, Snapshot, Invoke,
Reconcile, and Cancel. Reuse Connect over a bounded Unix socket and the READY
handshake. Domain APIs retain typed semantics. The generic host authenticates a
spoke with a per-launch scoped grant and routes declared capability requests;
spokes receive neither peer sockets nor raw database handles. Expired grants,
unknown methods, unmet capabilities, unavailable policy, and unknown stop state
refuse effectful calls. New general-purpose methods must not weaken the existing
typed authorization boundary.
Do not hold the hub registry or dispatch lock across an external spoke call.
An Invoke that produces a report or requests a hub capability before replying
must complete within its deadline; callbacks must not deadlock the action path.

The registry records package/configuration digest, binding identity/generation,
desired and observed state, executable ownership, capability grants, heartbeat
time, and failure/restart history. Expose Installed, Starting, Ready, Draining,
Failed, and Retired states separately from existing binding states. The hub owns
start/stop and restart backoff. An old generation remains available for pinned
reconciliation; replacement directs only new work to the candidate.

`ReportEnvelope/v1` contains plugin ID, binding generation, schema namespace and
version, monotonic sequence, reported and hub-observed timestamps, optional
execution reference, and a bounded JSON payload. Initial payload limit is 64 KiB;
large outputs use confined artifact references. Views comprise Status, Metrics,
Table, Timeline, Alert, and ArtifactLink. No executable UI code or private hub
types cross the reporting boundary.

Deduplicate by binding and sequence; reject changed payload under an existing
identity. Stale generations cannot overwrite active snapshots. A sequence gap
requests a snapshot with a watermark; transient heartbeats are distinct from
durable business observations. An injected clock computes freshness; socket
presence is not proof of health. Unknown namespaces can display generic data;
incompatible view/schema versions produce a visible unsupported state.

Durable plugin state/events/operation receipts need canonical component-scoped
LedgerApi operations. Freeze an atomic `ComponentCommit` (expected revision,
state/checkpoint, events, receipts) and read methods at the design gate. Do not
invent fake running factory executions to store global reports or use unrestricted
kernel metadata as an undocumented plugin database. The ledger implementation
owns additive physical tables, compatible snapshot export/import, and quiescent
migration. Reuse existing ledger binding and Connect client/server wrappers;
plugin reporters do not open SQLite.

Actions carry plugin/binding pin, action ID, operation ID, input hash, deadline,
execution/scope reference, and grant. The hub journals intent before invoking the
plugin and reconciles unknown outcomes. Reports cannot issue actions. Human UI
controls and autonomous policy use the same validated action service; destructive
human actions retain their declared confirmation requirements.

HP exit builds the hub once, separately creates an unfamiliar plugin package,
installs it, displays data/health, invokes an action, restarts the hub, upgrades
the package, and rolls back with IDs/receipts preserved. Test both allowed and
refused actions and inject crash boundaries. Include registry representations
for current built-in components; full migration of every implementation is not
an implied scope increase in an individual child.

## SM contract: measured prompt/plugin mutation

`MutationCandidate/v1` identifies directive/version, target kind (prompt, skill,
or authorized plugin package), immutable baseline/candidate refs and digests,
proposal/task origin, protected constraints hash, bench pack revision, and budget.
It is immutable. Model output proposes content; it cannot declare its own pass,
change the oracle, grant authority, or activate itself.

Lifecycle: Proposed -> Built -> Evaluating -> Rejected or Qualified -> Canary ->
Accepted or RolledBack. Persist transitions and reasons through the hub ledger.
Unknown or incomplete evidence never qualifies. Stable operation IDs identify
build, evaluation, promotion, and rollback effects; crash recovery uses receipts
and pinned bindings.

Protected bench packs include immutable fixture/oracle manifests and a held-out
set. Paired trials use the same tasks, clock policy, model/role/binding pins,
budget, and trial count for baseline/candidate. Pin seeds where supported; record
remaining nondeterminism. Capture failures, successes, token/cash/quota usage,
duration, missing/partial observations, and confidence/noise bounds. Small
screening runs cannot alone establish a promotion win. Register the exact sample
size and noise margin in the gate's evaluation policy, not in mutable prompts.

Qualification requires deterministic acceptance, protected regression checks,
independent evaluator evidence where required, and a paired improvement past the
registered margin with no regression veto. Identical/noisy candidates cannot win
by default. Evaluator sessions receive immutable candidate/oracle refs, run
independently, and their spend counts toward the task allowance.

Proposal mining initially uses recorded failures and explicit directives. Dedup
by directive, target, baseline hash, failure class, and proposal revision. Emit
complete staged task descriptions with provenance, acceptance, scope, and budget;
do not release tasks merely because they are proposed. Preserve the existing
human staging policy until the explicit directive/promotion policy permits an
automatic class. Rejected candidates retain reasons for subsequent proposals.

Promotion checks evidence hashes and current baseline identity before journaling
an activation intent. Prompt activation selects an immutable genome reference;
plugin activation drains/stages via the common registry. Keep the previous
reference/package and state reconciliation path. Activation completes only with
a receipt; crash after effect but before receipt reconciles without activating
twice. Do not implement promotion as an unguarded Git merge or executable swap.

Canary policy pins workloads, bounds, and regression thresholds. A regression or
unknown essential evidence disables promotion and restores the prior accepted
binding/genome with a durable rollback receipt. Reconcile pinned work before
redispatch; reverting a binary never rewinds the live database schema.

SM exit demonstrates a qualified retained prompt/plugin improvement, a known-bad
candidate rejected before activation, and a forced bad canary automatically
rolled back. Exercise restart during evaluation, promotion, and rollback. Emit
plugin reports to the hub UI and compare ledger outcomes with active refs.
Default tests use scripted harnesses; gated live evidence reports actual costs
and may not substitute a known-answer scripted candidate for a measured win.

## Implementation session instructions

Read this document, `AGENTS.md`, the epic, the child, its design/notes, and closed
dependency receipts. Work only in the child's listed files; shared wiring edits
are limited to imports/registration needed for its public surface. You are not
alone in this workspace: accommodate others' changes and do not revert them.
Implementation sessions do not commit, close dependencies, change task acceptance,
or declare the entire epic complete.

Children use the CP headings and a factory intake fence. Difficulty is structural:
these seams have cross-module interfaces or process/recovery behavior, so use
`difficulty:high` even when Flash is the builder. Model choice is separate from
that label. The builder-only selection child targets the existing reviewed
`opencode-go/deepseek-v4.1-flash` route without overriding planner/reviewer roles.

Each child introduces a named behavioral test target. The acceptance command must
run nonzero tests, exercise a failure path, and compare against an independent
fixture/oracle; an empty name filter or compile-only test is insufficient. Do not
rewrite protected recovery fixtures or their hash manifest. Use `ScratchDir`
under `target/test-runs/`, existing pinned crates, and offline checks. Every live
inference smoke requires the existing explicit live gate and budget admission;
filing these tasks does not start paid sessions.

Test commands describe the target to implement, not a claim that it exists today.
Run the child checks and relevant existing suites. The epic evidence child runs
the whole workspace suite once after integration, records remaining deviations,
and distinguishes deterministic proof from paid live evidence. Leave a handoff
with changed files, contract/schema changes, commands/results, receipt refs,
unresolved limitations, and the next dependency that is now satisfied.

## Backlog integration

- Preserve existing M2 scope and append this first tranche; do not close M2 on
  partial standup evidence.
- `agalma-myx.11` and `.12` must consume the common plugin platform rather than
  introduce a new hardcoded routing component/supervisor/UI path. Reconcile the
  conflicting router fail-closed/v1-fallback policies before implementation.
- `agalma-fez` needs actual blocking edges to `agalma-3ej` and `agalma-ud1` as
  already stated in its description.
- `agalma-ud1` uses the existing bounded-path fix `agalma-acs`; retain its live
  smoke and default-flip acceptance. Do not file duplicate wire/default tickets.
- Keep the decomposition pilot separate from a full goal-to-execution proof:
  staging alone does not prove execution, review, or epic closure.
