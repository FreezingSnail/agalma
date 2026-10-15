# Self-mutation contracts, revision 1

Date: 2026-10-12. Design gate: `agalma-fjq`. This document makes the SF, HP,
and SM contracts in `self-mutation-standup.md` concrete. It specifies future
implementation; it does not claim that these APIs or guarantees already exist.
The accompanying schema and examples are in `contracts/self-mutation-v1/`.
Later operational and autonomy work is scoped in `self-mutation-roadmap.md`.

## Decisions and authority

The first automatic mutation target is a prompt or skill. Independently installed
plugins use the same evaluation machinery, but each executable package family
needs an explicit allowlist entry before automatic release or activation.
The user selected prompts/skills first and higher task success with regression
and spending limits on 2026-10-12. Cost and latency are bounds, not interchangeable
rewards for a quality regression. A new versioned policy can choose a different
objective; plugin mutation is a subsequent explicitly allowlisted rollout.

The existing human execution gate remains in force for the filed bootstrap Rust
tasks: each staged child is pinned by a native human execution-approval gate
(labels `human,execution-approval`, metadata `approval_for=<child>`, and a direct
`blocks` edge) that only an explicit human `bd gate resolve` clears; validator
decomposition approval, a `staged` label, or a cleared defer date grant no
execution authority. Separately, the SM.9 automatic release service may release a
task only for a class explicitly authorized by an active directive, after
decomposition and scope checks and a durable release receipt. A model cannot
supply that authority. This design session does not release tasks, activate a
genome, or authorize paid inference.

First-domain permissions:

| Target | Propose | Automatically release/build | Automatically activate |
|---|---|---|---|
| Allowlisted prompt/skill | Yes, within directive | After deterministic release checks | After qualification and canary |
| Allowlisted plugin package | Yes | Only if its directive explicitly enables this class | Same gates plus package/config/state compatibility |
| Hub/kernel/adapter/platform code | Yes, staged | Transitional human oversight | P1 evidence and code policy required |
| Protected oracle, grants, credentials, hard limits | No automatic mutation | No | No |

`TaskReleasePolicy/v1` pins policy ID/digest, active directive/version/digest,
allowed target kinds and exact path/prefix scopes, protected paths, maximum task
cash allowance and attempts, required decomposition approval and required
qualification-policy digest. The initial allowed kinds are prompt/skill only.
The seed directive allows the builder prompt and builder-context skills;
evaluator/reviewer/planner prompts, evaluation policy, AGENTS/platform rules and
protected suites remain denied.
A `TaskReleaseReceipt/v1` pins task ID, full source-definition digest, policy and
directive, allow/deny reason, operation ID and observed time. Commit the receipt
through component storage before projecting an allow into bd. Repeated projection
reconciles the same receipt; different input under the operation ID conflicts.

At claim/package admission, revalidate the active policy and compare the frozen
source definition with the release receipt. A bd edit between release and claim,
revocation, missing receipt, protected-scope expansion or budget change parks
before inference/effects. Decomposition labels, a cleared defer date, and a
resolved bootstrap human gate are not substitutes for a durable automated-release
receipt. The explicit human `bd gate resolve` bootstrap path remains separate and
recorded. SM.9 owns the policy and projection primitive; M3.1 extends it with
operator cadence and aggregate limits.

An expired, cancelled, superseded, or unavailable directive denies new effects.
Recovery can read evidence and reconcile already announced work; it does not
interpret a new directive as permission to replay an old unknown effect.

## Common encoding, errors, and resource bounds

Wire records use UTF-8 JSON and explicit `format` versions. IDs reuse canonical
contract wrappers internally. Timestamps are unsigned Unix milliseconds; money
is unsigned nanoUSD, with checked arithmetic. Decimal money in intake is converted
exactly or rejected; it is never silently rounded down.

New package, configuration, evidence, and input identities use `sha256:<64 lower
hex digits>`. FNV remains readable for legacy operation conflict detection; it is
not accepted as an artifact integrity digest. The implementation may expose a
small pure digest helper in `agalma-contracts`, using already locked
`ring = 0.17.14` from the approved TLS dependency closure. No dependency download
or package-version refresh is authorized by this design. SF.2 owns its direct
dependency declaration and golden vectors; other components import the public
helper. The lockfile may record the direct edge using existing versions/checksums.

`canonical-json/v1` recursively sorts object keys by UTF-8 byte order and writes
compact JSON with the pinned `serde_json` serializer. Array order and string bytes
are significant. Parsed integer and floating representations remain distinct;
NaN/infinity are invalid. Integrity-bearing v1 records use integer JSON numbers;
decimal metrics use strings plus units. Floating-point inputs to these records
are rejected, avoiding different language encodings in integrity checks. A
package's digest covers its canonical document excluding its own digest field.
Raw text/artifact references hash the exact bytes. Cross-language implementations
must pass the golden vectors, including Unicode, escaping, key order and numbers.
Serialization failure is an error, never the hash of an empty buffer.

Keep the four existing `ContractError` variants. New protocols return structured
detail containing `code`, `subject`, and `message` within the selected variant:

| Variant | Stable detail codes |
|---|---|
| `KnownFailure` | `InvalidRecord`, `IntegrityMismatch`, `ScopeDenied`, `AuthorityDenied`, `DeadlineExceeded`, `BudgetUnavailable`, `Stopped`, `StateUnavailable`, `LegacyInputsMissing` |
| `UnsupportedCapability` | `UnsupportedVersion`, `UnsupportedSchema`, `MissingCapability`, `UnboundedProvider` |
| `Conflict` | `RevisionConflict`, `IdentityConflict`, `StaleGeneration`, `BaselineChanged` |
| `UnknownOutcome` | `EffectUnconfirmed`, `TerminationUnconfirmed`, `UsageUnconfirmed` |

A transport timeout is `UnknownOutcome` when an announced effect could have
happened. A malformed request refused before dispatch is a known failure.
Readiness/backoff can retry; neither retries nor an override authorize an effect.
New plugin error messages encode canonical `FailureDetail/v1` JSON inside the
existing string payload; the outer error code remains the existing variant name.
The common client decodes its human-readable `message` for display. Legacy plain
messages remain inspectable; failure-detail parsing never grants retry authority.
Unrecognized control versions refuse effects before any mutation. Reporting can
display an unsupported schema without treating it as authorization.

Initial limits are policy data recorded in the policy digest:

| Item | Limit |
|---|---|
| One manifest/config/schema document | 256 KiB |
| One report/action input/event payload | 64 KiB canonical UTF-8 |
| One component checkpoint | 256 KiB |
| Events or receipts in one component commit | 100 of each |
| Task package metadata | 8 MiB; larger text becomes immutable references |
| Text item rendered inline | 1 MiB; larger items remain readable references |
| One protocol artifact | 16 MiB; larger results require a separately authorized artifact class |
| Generic hub action jobs | 32 outstanding, with explicit backpressure |

Artifact references resolve only within the admitted store and caller's readable
scope. Absolute paths, `..`, symlink escapes, mutable network URLs, and missing
digests are refused. A held-out artifact reference does not grant builder access.

## SF: immutable inputs and execution settlement

`TaskPackage/v1` is a manifest plus immutable referenced objects. The schema fixes
its required fields. `definition` includes the full title/body/design/notes,
machine block and acceptance. `context` includes instruction files, parent epic
design, shared contracts, and relevant closed-dependency handoffs, each with
source identity and a digest. Source identity is provenance, not a recovery lookup.
An explicit empty section is valid; an omitted required section is not.

Role pins contain the actual route/model and harness binding selected for each
role that may run. Prompt/configuration references are also immutable. Retry
artifacts are append-only attempt inputs; they cannot replace the original
acceptance, constraints, model pins, or oracle. Different attempts get distinct
input identities derived from the package and admitted retry references.
New admission reads the accepted immutable genome reference. Promotion selects
that reference for new sessions; it does not overwrite a compiled prompt or
require rebuilding the hub. A compiled seed prompt can initialize the first
genome; already admitted packages keep their original reference.

The package's authorization records human-bootstrap versus directive-policy
origin, policy digest and immutable release/admission receipt. Receipt integrity
alone is insufficient: admission verifies its trusted ledger provenance, active
directive and matching full source-definition digest before dispatch. A model
cannot fabricate a grant by writing a correctly hashed receipt artifact.

Admission order is normative:

1. Claim the queue task and allocate its execution identity and lease generation.
2. Resolve source SHA, scope, directive, role pins and all context once. Copy
   referenced bytes into a hub-owned content-addressed artifact store.
3. Write each object to a unique temporary file, flush, atomically publish it,
   and flush its directory. Verify existing objects before reusing them.
4. Atomically commit the execution record and `task.package_admitted` event with
   package reference/digest and lease. No dispatch intent precedes this commit.
5. Dispatch only from that durable package. A crash before step 4 can leave
   unreferenced objects, which are harmless and later collectible.

An execution ID allocated during preparation is not itself dispatch permission.
If the current engine cannot atomically combine start and the admission event,
it creates a non-dispatchable preparation state and must commit the admission
before calling any phase handler. Recovery of that preparation never invents
historical inputs from current bd content.

Recovery validates package and artifact integrity, history/projection agreement,
lease and binding pins, then reconciles only that execution's survivors. Missing
legacy in-flight packages park with `LegacyInputsMissing`; terminal history stays
inspectable. Global recovery cannot probe one execution with another's activity
registry. The first SF tranche may retain the seed driver, but its evidence must
name that fact; production kernel dispatch is separate convergence work.

Cancellation uses a monotonic settlement record, not a new global pause:

| Cancellation state | Meaning |
|---|---|
| `none` | No cancellation request recorded |
| `requested` | Durable `execution.cancel_requested`; all new dispatch/advancement denied |
| `settling` | Cleanup and reconciliation underway; operation outcomes can remain unknown |
| `settled` | All survivors confirmed terminal, pending intents consumed with cancellation evidence |

The settled public projection is existing `ExecutionState::Failed`, phase
`Parked`, with outcome/reason `Cancelled`. Kernel-native `ExecState::Cancelled`
maps to that projection. This avoids adding an enum unknown to existing schema-v4
readers. Pending cancellation remains visibly unsettled even if its coarse
execution row was parked after a cleanup error. Status must expose the settlement
record; the coarse terminal bit alone is never termination evidence.

Journal the request before worker termination; reconcile effects that may already
have happened, including integration. Cancellation does not undo an already
completed merge or refund provider charges. An unconfirmed worker or operation
keeps settlement pending, returns `UnknownOutcome`, and cannot be redispatched.
Once requested, resume cannot remove cancellation. A new execution generation
requires a new authorized claim. Duplicate cancel returns the durable status.
Already completed work retains its completed outcome with a no-op cancel receipt.

SF.4 owns the seed and kernel adapters, cancellation transition/cleanup wiring,
projection mapping, and status rendering. Existing protected recovery fixtures
remain untouched; new golden cancellation histories cover the revised behavior.
Cleanup/reconciliation is allowed while globally stopped because it reduces
existing exposure. New claims, planning inference, builds, integration, and
effectful plugin actions require an explicit `Running` stop observation; `Stopped`
and `Unavailable` deny them. First-open empty state is explicitly initialized
before admission, rather than treating every ledger read failure as an empty DB.

Scope is a normalized repository-relative exact-path/prefix allowlist plus a
protected denylist; the denylist wins. Check both sides of a rename, deletions,
mode changes, symlinks, generated outputs and nested repositories. Acceptance
commands stay pinned strings using the existing scope-checked command runner.
Builder visibility excludes held-out answers. Verification runs in a separate
read-only-oracle sandbox against a pinned candidate SHA and records both digests.

## SF: enforceable request budgets

The Router chooses eligible routes; the parent request admission service enforces
budgets. A statistical p95 estimate may pace work but is not a hard reservation.
`--model` and `--builder-model` select a candidate route; they cannot bypass
credentials, capability, scope, billing, stop, or budget checks.

For one admitted request, reserve the reviewed maximum billable input and output,
not an estimate of the prompt's current token count:

```
reserve_nusd = ceil((input_limit * max_input_cash_price_per_million_nusd
                  + output_limit * max_output_cash_price_per_million_nusd) / 1_000_000)
             + maximum_other_charge_nusd
```

The reviewed profile pins provider endpoint/model, billing revision and source
evidence, validity interval, maximum price over the entire request interval,
billable context bound, enforceable output bound, exact limit field, allowed
request kinds, `n = 1`, and all additional billing features. Unsupported hidden
reasoning limits, media/tool charges, endpoint behavior or pricing deny admission.
Price windows reserve the worst applicable rate; an expired profile is unusable.

Use inspected HTTP-forward mode for bounded inference. Buffer a bounded request
body before contacting upstream, reject unsupported paths/features, enforce its
limits, commit its reservation, then send once. CONNECT and arbitrary streaming
request bodies cannot provide inspected inference guarantees and are refused for
such routes. Explicitly separated non-billable metadata traffic has its own
allowlist. Worker-supplied accounting IDs cannot change execution/role attribution;
the parent proxy instance pins that scope. SDK retries are new admitted requests.

`spent + outstanding + proposed_reservation <= allowance` is checked atomically
using ledger revisions, across all roles/attempts in the execution. Reservations
record request ID, package, role, route/profile digest, maximum liability and
state (`reserved`, `sent`, `settled`, `unconfirmed`). A duplicate reservation with
the same input returns its receipt; changed input conflicts. Never automatically
resend a `sent` request after proxy restart without provider idempotency evidence.

Complete authoritative usage can release unused reservation. Unknown/partial
usage retains the full liability and denies subsequent paid work in that scope
until reconciled. Provider charges may arrive after cancellation; an admitted
request's maximum remains reserved until settled. Report cash and subscription
quota separately. Cash rates describe additional cash liability; the separate
quota-policy reference defines reservations in each bucket's native unit. Never
add percent/credits to nanoUSD or count subscription allowance as another cash
charge. Zero additional cash does not mean unlimited quota or zero compute cost.
Unknown quota denies a route governed by a quota cap.

The design example has a fake byte-token provider: 4,096 input tokens, 1,024
output tokens, input rate 2,000,000,000 nanoUSD per million, output rate
6,000,000,000, no other charges. Its exact reservation is 14,336,000 nanoUSD.
It is a test profile, not a price claim about Flash or another live provider.
No live provider profile is certified by this review. SF.7 must produce the
reviewed profile and adapter limit evidence before SF.9's live portion can run;
an unsupported live profile remains ineligible. Deterministic implementation
tests use the concrete fake bounds above and independently specified failures.

## HP: package identity, discovery and lifecycle

A package is a sealed directory: manifest, executable, configuration schema,
default configuration and any resources. Installation configuration is separate:
`PluginInstallation/v1` pins the package, actual immutable configuration ref,
launch profile and approved capability set. Changing configuration creates a
binding generation without editing or rebuilding the package or hub. The manifest
pins its default configuration, not the operator's mutable current configuration.
The package digest covers an ordered file inventory containing
relative path, mode and raw-byte SHA-256, plus the canonical manifest excluding
its package-digest field. Symlinks escaping the package are invalid. Rehash on
install, stage and launch; copy installed generations into hub-owned immutable
storage, so editing a discovery directory cannot change a pinned process.

The manifest separates offered domain APIs from requested capabilities. A package
may implement generic reporting without becoming a workflow participant. Required
services resolve through the hub; optional unavailable services are visible.
Duplicate plugin IDs are conflicts, even if two discovery roots contain identical
packages. Selection/replacement is an explicit registry operation, not directory
order. Unknown API majors and required-dependency cycles refuse activation.

Registry generations are monotonic per plugin and never reused after rollback.
Package/config/capability-scope changes create a generation. Rotating a secret
for a new launch does not change the logical binding: a distinct launch epoch
fences the old process, reissues the same bounded scopes, and lets the identical
package reconcile its pinned generation. Installed/Starting/Ready/
Draining/Failed/Retired are observed lifecycle states, separate from binding
Staged/Active/Draining/Retired. Desired state is stored durably. Mark `Ready` only
after handshake identity, Describe compatibility, grants and Health succeed.

Defaults: startup deadline 10 seconds, Health poll 5 seconds, stale after 15
seconds, failed after 30 seconds without successful observation. Restart backoff
is 1, 2, 4, 8, 16, then 30 seconds; five failed launches in five minutes open
the circuit. A health success lasting 60 seconds resets backoff. Test clocks are
injected. Policy versions may tune these values within platform bounds.

Staging validates package/config compatibility before draining the active
generation. New work switches only after the candidate is Ready and activation
is durably receipted. Draining generations accept reconcile/cancel/read requests
for their pinned operations. A drain timeout reports unresolved work; it does
not send unknown effects to the replacement. Removal refuses unresolved pins.
Rollback restores the prior compatible package/config with a new generation;
already announced operations retain their original generation and receipts.

## HP: common RPC and callback behavior

Spoke methods are `plugin.Describe`, `plugin.Health`, `plugin.Snapshot`,
`plugin.Invoke`, `plugin.Reconcile`, and `plugin.Cancel`. Hub methods include
`hub.PublishReport`, `hub.InvokeCapability`, and `hub.ReconcileCapability`.
Domain capability adapters validate canonical typed inputs, not arbitrary shell
commands or arbitrary LedgerApi method names.

Grant tokens are 32 random bytes, created parent-side per launch, transmitted
through a private launch channel, and never stored in reports/receipts/logs.
The hub stores a grant fingerprint and its scope: plugin, generation, method,
resource namespace, expiry and active directive where relevant. The authenticated
grant determines caller identity; supplied plugin IDs do not override it.
Single-user local UI requests use the existing human actor and confirmations.
Capability grants do not let a spoke resume the factory or alter its own grants.

`Invoke` validates and durably journals the operation, then returns an accepted
acknowledgement with operation identity. Effects execute in bounded jobs outside
the transport dispatcher and registry locks. `Reconcile` returns
`accepted/running/completed/failed/cancel_requested/cancelled/unknown` and a
durable receipt when terminal. The same protocol applies to effectful hub
capabilities. A model marker is never an operation receipt.

HP.4 introduces a concurrent plugin dispatcher over the existing Connect
envelope: copy an immutable handler reference while locked, release the lock,
then execute it. It must not call an external service under the generic
`Server::dispatch` mutex. Durable domain operation IDs provide effect dedup;
the current process-local numeric request cache is not a durable action journal
and must not be shared across unrelated plugin clients. Read requests need no
effect cache. Existing framed/server semantics remain supported for old domains.

An action may report or invoke an authorized hub capability before completion.
The bounded test starts that callback before the action can finish, then proves
both complete before an injected deadline. Separate nested capabilities have
their own operation IDs. A spoke cannot recursively authorize the same operation;
cycle/depth limits deny such calls. Deadline expiry stops new execution and
requests cancellation; it does not manufacture evidence that the process died.

## HP: durable component storage and reports

`ComponentCommit/v1` is an atomic component-namespace transaction: commit ID,
input identity, expected revision, optional checkpoint, events, operations and
receipts. Operation records include pins, intent, state and deadline. The hub
derives and authorizes the namespace. A plugin cannot select another namespace
or use global kernel metadata. Component data does not require fake executions.
Commit replay anchors are durable component data too; snapshot/restore retains
their input identity and original commit receipt, not only effect receipts.

Replaying a commit ID with identical canonical content returns the original
commit receipt before checking the now-changed revision. Different content under
that ID conflicts. New commits require an exact expected revision; missing means
revision zero only for initial creation. Either all rows commit or none do. Event
sequence and new revision are ledger-assigned. Duplicate operation/receipt IDs
must have identical input/pin/outcome content. No external I/O runs inside a DB
transaction. A state change and the receipt proving it can share one commit.

Read methods are `ComponentRead(namespace)`, `ComponentEvents(namespace, after,
limit)`, and `ComponentOperation(namespace, operation_id)`. They return revision,
watermark and immutable references. HP.5 owns the implementation and both ledger
transports. Component writes require negotiated `component-store/v1` capability;
old ledger bindings refuse them before dispatch.

The ledger schema is currently v4. HP.5 adds v5 component checkpoints/events/
operations/receipts and extends export/import to `agalma-state/v2`. Upgrade is an
explicit quiescent migration with backup, exclusive writer ownership, no live
workers or unresolved effects, additive DDL in one transaction and a migration
receipt. A parked row alone does not establish quiescence. Normal opening of a
v4 ledger does not silently perform this new migration.

The current v1 logical snapshot omits decisions, decision outcomes, digests and
control receipts. The complete v2 format must include those four sections as well
as all existing sections and new component state/events/operations/receipts/commit
anchors. Fields are enumerated in `contracts/self-mutation-v1/snapshot-sections.json`.
HP.1 defines their canonical row types; HP.5 proves nonempty round trips and
duplicate-control/decision identity after restore. A legacy v1 export alone cannot
prove those sections were empty. Conversion for production recovery requires a
quiescent original ledger or a complete same-watermark supplement. Legacy v1
methods remain supported for existing fixtures/inspection; production backup and
replacement negotiate a separate complete-v2 export/import capability.

Normal v2 import requires the current format/schema and an empty destination,
validates everything first, and commits atomically. Unknown formats/sections refuse.
Old binaries remain incapable of writing v5. A plugin rollback uses the current
ledger service; a ledger binary rollback requires a tested v5-compatible writer
or a separate restore drill. Never restore an old DB over newer external effects.

Report identity is `(plugin_id, generation, sequence)`. Sequence is durable and
monotonic within a generation; an exact replay acknowledges without appending.
Changed payload at the same identity conflicts. Reports carry their schema,
observed/reported time and optional execution ref. The hub's injected clock sets
freshness. Retired generations can complete their pinned observations but cannot
overwrite the active view.

A gap marks the view incomplete and schedules Snapshot. A snapshot includes a
watermark and digest; it replaces the active read model at that watermark, then
applies queued later reports. Out-of-order duplicates below the watermark do not
roll the view back. Bound the queue; overflow discards transient data and retries
a snapshot while retaining the visible incomplete state. Durable observations
must be committed before acknowledging publication; heartbeats can be transient.

PublishReport accepts `ReportPublication/v1`, without a hub-observed timestamp.
The stored `ReportEnvelope/v1` adds that timestamp. Dedup compares producer input
and returns the original observation; a retry does not assign a new observation
time. Snapshot returns namespace/schema payloads at a global publication watermark
and is bounded to 256 KiB; oversized data uses confined immutable refs.

Health returns identity/generation, `ready/degraded/unavailable`, detail and its
reported check time. The hub records its own observation time and applies the
freshness policy. Describe returns the manifest and actual bound identity.
Invoke/Reconcile/Cancel return `PluginOperationStatus/v1`: pinned identity,
operation/input hash, state and nullable durable terminal receipt. Cancel has a
separate stable cancel-request ID, original operation ID, deadline and grant
fingerprint; changed content under the cancel ID conflicts. These methods do not
depend on numeric transport request IDs for durable identity.

Views use only Status, Metrics, Table, Timeline, Alert and ArtifactLink. They
contain field paths and text, no scripts/templates/SQL/remote fetch. Escape control
characters and bound rows/text. Unknown private namespaces render generic data;
unsupported view versions are visible. Actions are separately advertised and
authorized; a report never schedules work or calls an action.

The hub journals action intent before invoking a pinned plugin and commits its
receipt before declaring completion. A lost acknowledgement reconciles the same
operation on the same generation. If that generation cannot reconcile, the action
stays unknown; replacement is not permission to perform it again.

## SM: qualification and canary policy

`MutationCandidate/v1` is immutable. Its baseline, candidate, directive,
constraints, bench pack, budget and target pins are mandatory. Provenance links
the proposal and build task. Lifecycle is Proposed -> Built -> Evaluating ->
Rejected or Qualified -> Canary -> Accepted or RolledBack. State transitions,
reasons and evidence identities are atomic ledger component commits.

The initial `success-first/v1` profile is fixed before any candidate sees results:

| Rule | Initial value |
|---|---|
| Screening | 10 public tasks, 2 paired trials; never establishes qualification |
| Qualification | 20 protected tasks, 5 paired trials each; 5 tasks held out from proposal/build |
| Pair identity | Same task/seed policy, models, role/binding/config pins, limits and oracle |
| Primary margin | Candidate mean task-success rate at least 5 percentage points higher |
| Noise rule | Exact one-sided sign test over per-task paired mean differences, p <= 0.05; ties excluded |
| Regression veto | All deterministic protected checks pass; no declared critical task degrades |
| Economics bounds | Complete cash/quota evidence; each cap respected; each mean spend dimension <= 110% baseline |
| Latency bound | Mean wall time <= 120% baseline, with timeout failures counted |
| Evaluator | Independently selected eligible role; fixed rubric; missing/disagreement refuses qualification |

The experimental unit is a task, not each of its repeated trials. Persist raw
trials and per-task aggregates. Report the sign-test count, p value, margin and
effect distribution; do not claim population-wide confidence from a deliberately
curated fixture pack. A no-op must not qualify. A zero-spend baseline requires
zero candidate variable spend rather than division by zero. Compare cash and each
quota bucket in their own units using the same role/model profiles; do not convert
unknown subscription use into fictional cash. Unknown usage is not a cheap win.

Each task alternates baseline-first/candidate-first deterministically from its
trial identity. Seeds are pinned where supported; unsupported seeds remain
explicit nondeterminism. Failed trials count as failures, never disappear from
the denominator. Missing trials/evidence prevent qualification. The fixed total
budget reserves both arms, evaluators and overhead; an unaffordable full pack
does not fall back to a screening win. Fixture generation and real inference
evidence are distinguishable in every result.

For a future cost-first policy, success must be noninferior under a preregistered
quality test and spending must improve beyond its noise margin. That is a new
policy revision, not an implementation-session switch.

Promotion is compare-and-set against the accepted baseline and evidence hashes.
It selects an immutable genome or stages a compatible plugin; it never edits a
running prompt file in place or blindly swaps an executable. Retain prior refs,
package/config and the current ledger/state reconciliation path. Unknown
activation outcomes reconcile before any second activation attempt.

Initial canary: at most 10 candidate and 10 baseline eligible real tasks,
alternated serially, with a 24-hour deadline and a separately admitted aggregate
budget. A protected fixture probe runs at activation and after each five real
tasks; probe costs are included. Critical regression, authority/scope breach,
unknown essential usage/state, or two candidate verification failures triggers
immediate rollback and inhibits further promotion. Each execution also keeps its
own spending and deadline bounds. Canary acceptance requires all 20 results,
passing probes, no trigger and an independent receipt. Deadline/budget exhaustion
before completeness restores the previous ref with `InsufficientCanaryEvidence`.
This canary detects degradation; it does not replace the full qualification test.

Rollback journals intent, restores the prior compatible ref through the same
registry/genome service, reconciles candidate-pinned work, and records a receipt.
It never rewinds the live ledger schema. Policy/grant changes require a new
directive or platform policy; candidate code cannot redefine a rollback trigger.

## Prerequisite-aware epic review

SF.11 freezes the missing review admission rule. `EpicReviewAdmission/v1` is the
typed projection in the shared schema/example: epic/source identity, immutable
design, base/candidate SHAs, complete children with definition/status/labels,
direct edges with target/type/nullable observed status, decision/blocking IDs,
observation time and canonical input digest. Only `blocks` prerequisites
gate review; `tracks` and parent-child edges do not. Missing targets, malformed
graphs, cycles or unavailable queue data deny reviewer dispatch. An open blocking
prerequisite produces `WaitingPrerequisite` with its IDs, not a drift verdict,
rework task, retry charge or global drift hold. Previously recorded genuine drift
is not cleared by this check.

Require all implementation children to be closed, including deferred children,
and at least one staged child. A zero-child epic is never a review candidate.
Recheck the complete child set and prerequisites before paying for review and
before applying approval/closure. Changed design, children or blocking edges
invalidate the approval input identity; record a stale admission without closing.
The reviewer input pins the admitted graph and immutable design/commit references.
No prerequisite snapshot is durable authorization for later queue edits.

The typed method is `TaskQueueApi::review_admission(epic_id, context)`; context
contains the immutable design reference and base/candidate SHAs. The bd adapter
reads the complete current source/children/edge projection and computes its
deterministic decision; the caller supplies admitted immutable context, not child
status or an eligibility override. The IPC method is `review_admission`, with
`{id, context: {design, base_sha, candidate_sha}}` parameters and the versioned
record result. SF.11 owns both task-queue transports. CLI parsing stays in the
adapter. A default unsupported implementation preserves old trait callers but
denies automated review until `review-admission/v1` is negotiated. The conductor
verifies that the design reference matches the source definition before using it.
M2's `blocks` edge to its extensions epic is deliberate: the first SM tranche may
finish without pretending that nerve/onboarding/memory obligations are complete.
SF.10's fresh pipeline proof covers waiting, prerequisite completion, one review
and closure, plus a graph edit between reviewer dispatch and approval.

## Closure and evidence

SF closes on package delivery, protected behavior, recovery/cancel/budget and
role-routing evidence through the real work path. HP closes on an unfamiliar
independently built package, unchanged hub binary digest, generic UI/reporting,
allowed/denied actions, restart, upgrade and rollback with IDs intact. SM closes
its first tranche on a retained qualified win, rejected bad candidate, and bad
canary rollback. Scripted protocols prove mechanics; actual model improvement
needs complete live observations under an admitted policy.

These exits do not close M2's nerve/onboarding/memory obligations, M3's 72-hour
operations proof, or P1's removal of transitional human code oversight.
