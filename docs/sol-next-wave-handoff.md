# Sol next-wave planning handoff

Prepared 2026-10-13 while the authorized OpenCode batch runs. This document adds
ownership and review sequencing to existing accepted beads; it does not replace
their designs, frozen inputs or acceptance. Current session IDs and review
criteria are in [the batch handoff](opencode-parallel-batch-1.md).

## Readiness and shared ownership

2026-10-13 next-wave update: the completed repair review found SF.2/SF.5 still
incomplete. The user's next DeepSeek wave resumes those two existing sessions
under the bounded ownership amendments in docs/opencode-parallel-batch-1.md,
Next repair wave authorization and ownership. That resolves the executor
preparation and established-ledger opening blockers before the new SF lanes
below. No deferred SF/HP/WEB child is released by this repair dispatch.

SF.1, SF.3, SF.6 and SF.8 are deferred/staged and each has a direct SF.2 blocker.
Their shared design gate agalma-fjq is closed. Preparation does not release them.
Before a new dispatch, refresh compact blocker/release state and use SF.2's
reviewed public package surface. Do not guess an interface while Lane A edits it.

| Task | Work that can be separated after SF.2 | Integration ownership |
|---|---|---|
| agalma-5fy.1 / SF.1 | Repository/fixture prompt text, composition and prompt tests | phases.rs exclusive to SF.1 during its integration |
| agalma-5fy.3 / SF.3 | Execution-scoped executor recovery and recovery tests | work.rs after the current SF.5 integration is reviewed |
| agalma-5fy.8 / SF.8 | CLI/config/routing selection and route tests | work.rs after SF.3; task_package.rs after SF.2 releases it |
| agalma-5fy.6 / SF.6 | Scope guard, typed Workspace inspection with IPC parity and independent tests | work.rs after SF.8; phases.rs after SF.1 |

This is the proposed next-wave ownership order, not new semantic dependency
edges. Separate files can progress concurrently only under an explicit bounded
assignment. Each integration waits for the preceding owner to hand off, Sol to
review it, and an explicit ownership transfer. Do not mark a partial lane complete.
Keep scope changes in SF.6 separate from SF.3 recovery and SF.8 route selection.

Each assignment uses GPT-6 Luna or DeepSeek Flash through OpenCode, names its
bead and this section, and points to AGENTS.md, docs/development-workflow.md,
docs/self-mutation-standup.md, docs/self-mutation-contracts.md and the complete
assigned bead. Implementers read their own task locally without expanding every
parent/dependency body. Preserve complete frozen execution context separately.

Preserve existing/concurrent edits, use offline dependencies and the established
cache, and put raw logs/exact commands/exit status in
target/test-runs/<bead-id>/. Use one focused command per task as specified below.
No paid-provider acceptance, commit, push, task closure or further release is
part of these prepared handoffs. Stop for review after the bounded implementation
and evidence, or after one reproducible external/contract blocker and at most
30 minutes of diagnosis. Shared-file integration that is held must be reported
as incomplete, with the exact proposed change.

## agalma-5fy.1 — complete frozen builder context

Consume the reviewed TaskPackage, never a live bd lookup. Deliver the task's
scope/files/contracts/invariants/non-goals, full design/notes, frozen repository
rules, relevant parent/shared/dependency handoffs and admitted retry diagnosis.
Keep fixture and repository prompt selection explicit and preserve file-only
retry handoff. Missing required context is a known failure, without fallback to
the fixture prompt. An explicitly empty section remains valid.

Own conductor prompts/builder.md, prompts/builder-fixture.md, src/composition.rs,
src/phases.rs and tests/sf_builder_prompt.rs. Preserve FEZ edits in phases.rs.
Do not edit work.rs or package schema, scheduling, route eligibility or acceptance.

Focused command: `cargo test --offline -p agalma-conductor --test sf_builder_prompt`.
Review requires independently expected prompt fragments, a multi-file feature,
missing-context refusal, immutable rule bytes, admitted retry diagnosis and
unchanged M0 fixture behavior. Include a rendered prompt artifact for SF.9.

## agalma-5fy.3 — execution-scoped recovery gate

Reuse the existing reconstruction/effect probes with an execution-scoped entry
point. Before recovered dispatch, validate recorded history against projection,
restore activity context from the package and reconcile only that execution's
survivors on pinned bindings. Confirm survivor termination; ambiguous evidence
parks without redispatch. Preserve version gates, leases and protected recovery
fixtures. Keep the existing driver; production kernel replacement belongs to CV.

Own crates/agalma-execution/src/executor.rs and conductor tests/sf_recovery_gate.rs.
The bead's conductor src/work.rs integration is held until SF.5 releases it.
Do not implement SF.4 cancellation or alter another execution's context.

Focused command: `cargo test --offline -p agalma-conductor --test sf_recovery_gate`.
Review requires a history-preserving projection tamper with zero later effects,
two executions with distinct workspaces/acceptance and task-scoped probes,
crash-after-effect reconciliation without repetition, and ambiguous survivor
refusal. A call to a global recover method or a compile-only result is insufficient.

## agalma-5fy.8 — builder-only model selection

Add the accepted builder-only CLI/config selector. Explicit builder selection
wins over legacy global selection for builder only; planner/reviewer/verifier
keep their independently resolved routes. Preserve legacy --model behavior,
credential/capability gates and immutable role pins across restart. The reviewed
Flash identifier is opencode-go/deepseek-v4.1-flash. Do not mutate the catalog or
introduce smart routing v2.

Own conductor src/cli.rs, src/config.rs, src/routing.rs and tests/sf_builder_route.rs.
Own src/task_package.rs only after SF.2 releases it. The bead's src/work.rs
integration follows SF.3 under explicit transfer. Do not alter SF.3 recovery gates.

Focused command: `cargo test --offline -p agalma-conductor --test sf_builder_route`.
Review requires independently resolved non-builder roles, selector precedence,
unset/legacy compatibility, missing-credential refusal, recorded provenance and
restart after environment changes. Use fake policies; paid inference is unnecessary.

## agalma-5fy.6 — candidate scope and protected oracle

Authorize candidate adds/modifications/deletions/renames/mode changes against the
package's normalized exact-path/prefix allowlist and protected denylist; deny wins.
Check both rename sides, symlink escapes and generated/nested repository paths.
Supply immutable baseline oracle inputs read-only through typed WorkspaceApi and
SandboxApi. Pin candidate SHA and oracle digest at verification and revalidate
identity before integration. Candidate-authored replacement tests cannot be the
sole judge. Keep held-out inputs inaccessible to the builder.

Own conductor src/scope_guard.rs and tests/sf_scope_guard.rs, contracts src/workspace.rs,
workspace src/lib.rs and ipc src/workspace_client.rs/src/workspace_server.rs.
The bead's conductor src/phases.rs follows SF.1 and src/work.rs follows SF.8.
Keep Workspace transport parity; preserve existing protected recovery fixtures.

Focused command: `cargo test --offline -p agalma-conductor --test sf_scope_guard`.
Review requires malicious always-pass candidate tests rejected by independent
protected inputs, escaped paths/renames/deletions/symlinks/mode changes refused,
a permitted source feature passing, and changed SHA between verify/integrate
refused. Evidence names candidate SHA, oracle hash and denied paths.

## Plugin platform contract readiness

HP.1 (agalma-jmr.1) remains deferred behind SF.9 (agalma-5fy.9) and SF.10
(agalma-5fy.10), plus the closed design gate. Its accepted contract is already
defined in docs/self-mutation-contracts.md and docs/contracts/self-mutation-v1/schema.json;
this preparation introduces no new protocol or framework.

| Boundary | Existing contract to preserve | Implementation owner / evidence |
|---|---|---|
| Package and installation | Sealed inventory digest, separate immutable operator config/grants, versioned manifest and typed offered/requested APIs | HP.1 types; HP.2 discovery refusal cases |
| Registry and supervision | Monotonic generations, separate launch epoch, desired/observed state, handshake/health readiness and pinned draining operations | HP.3 lifecycle and replacement tests |
| RPC and capability routing | Describe/Health/Snapshot/Invoke/Reconcile/Cancel; scoped grants; accepted acknowledgement and durable reconciliation; no external calls under dispatcher locks | HP.4 callback-before-completion and authority tests |
| Reports and durable storage | Producer publication versus hub-observed envelope, generation/sequence dedup, snapshot watermark recovery, atomic component commits and complete-v2 export/import | HP.5 replay, gaps, nonempty round trips and quiescent migration |
| Dynamic UI | Registry-driven navigation; Status/Metrics/Table/Timeline/Alert/ArtifactLink; explicit stale/disconnected/unsupported states | HP.6 unknown namespace without custom hub UI |
| Actions | Pinned operation/input/deadline identity, intent before effect, durable receipt, cancel and unknown-outcome reconciliation; common stop guard | HP.7 lost acknowledgement/crash/refusal tests |

HP.1's owned files remain contracts src/plugin.rs, src/ledger.rs, src/snapshot.rs,
src/lib.rs and tests/plugin_protocol.rs. Its focused command is
`cargo test --offline -p agalma-contracts --test plugin_protocol`.
Review independent golden/malformed fixtures, supported/unsupported majors,
missing grants/capabilities, bounded report identity/payload, changed action input,
component checkpoints and complete snapshot row types. Common contracts contain
no I/O, vendor handles or implementation imports. Reuse canonical FailureDetail,
digest/error conventions and policy bounds; plugin-private data stays namespaced.

Platform implementation order remains HP.1 → HP.2 → HP.3 → HP.4 → HP.5, then
HP.6 and HP.7 can overlap subject to owned UI files, followed by HP.8. SF.5's
stop semantics are reused by actions. Router extraction consumes the proven
platform. HP.1 contract checks alone do not establish the HP.8 acceptance:
install an unknown plugin, display its data/health, invoke a declared action,
restart/upgrade/rollback with identity and receipts retained, without a hub rebuild.

The 2026-10-13 readiness amendments are in
[hp-bead-readiness-review.md](hp-bead-readiness-review.md): production wiring
ownership, root failure policy, explicit migration/restore and callback cases,
HP.6-to-HP.7 watch.rs handoff, and the independent HP.8 fixture build. The eight
existing beads carry these additions; their defer state and dependency graph are
unchanged. This is a readiness review, not implementation acceptance or release.
