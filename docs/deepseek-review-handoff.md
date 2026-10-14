# DeepSeek implementation review and handoff

Reviewed 2026-10-12. This supplements the frozen revision-1 contracts and
`self-mutation-design-receipt.json`. Original document hashes remain unchanged.
The review updates planning records and acceptance context; implementation files
remain owned by the sessions working their beads.

## Current order

| Bead | Review and next action |
|---|---|
| `agalma-cmq` | Closed at `3ddcd980d190868824169d9a380cee235c862555`; bounded timeout change reviewed without finding scope drift |
| `agalma-9m2` | Closed during this review; the tested scoped CLI handoff now satisfies SF.5's added prerequisite |
| `agalma-fez` | Closed and independently verified on resumption: five fresh smokes; 335 conductor unit/integration checks passed, two live-provider cases ignored. Receipt: `fez-acceptance-receipt.json` |
| `agalma-5fy.2` | First new standup task with closed prerequisites; still deferred; deliver complete immutable builder/recovery inputs |
| `agalma-5fy.5` | Still deferred; `agalma-9m2` is now closed, so its added prerequisite is satisfied |

The original standup index records the first filing graph. The amended SF.5
ownership/order is pinned in `self-mutation-design-amendment-1.json` and the
current bead. SF.5 now owns `src/designer.rs` in addition to its original files.
All 61 standup/roadmap implementation children remain deferred. No release or
paid inference follows from this review.

## Reviewed behavior and limits

The timeout change separates `DesignerOutcome::TimedOut` from completion and
validates committed children in the same pass. Full commits approve without a
second designer session; partial/empty commits count toward the bounded retry
policy; restart retains child identity and recovers interrupted projection.
The designer-specific timeout override leaves the worker/concierge/reviewer
default unchanged. Shared session cleanup still ignores cancellation/close/stop
errors. This review does not establish durable termination or settlement.

The scoped CLI reuses `run_epic_turn` and refuses missing, closed, human-escalated,
approved and hand-authored epics. It does not replan unrelated epics. Its current
wrapper starts the provider proxy before `run_designer` checks stop. SF.5 must
consume Running/Stopped/Unavailable before proxy/session construction and new
queue effects, and test the fleet and scoped entry points. An existing STOP
sentinel test does not establish behavior for unavailable ledger state.

Five new process smoke tests currently return successfully when a required
server executable is missing. A passing test count can therefore conceal a
skipped boundary. FEZ's required evidence path must fail on missing binaries,
or use a runner that verifies/builds every executable and rejects skipped cases.
The five server executables are present in this review environment. Restricted
sandbox execution denied Unix socket binding; the smoke command is rerun with
approved permissions, not classified as a product regression.

Ledger/workspace/taskqueue process-default changes do not establish common
plugin migration or removal of auxiliary SQLite consumers. Sandbox is already
process-based. Execution remains a candidate with its default explicitly
deferred; CV.1 owns production selection of the kernel binding.

## SF.11 review identity clarification

An `EpicReviewAdmission/v1` record hashes its complete immutable contents,
including observation time. A fresh read naturally has a different observation
time and full-record digest. Those differences alone cannot invalidate review.
Verify each record's integrity, then compare its semantic review inputs:
`epic_id`, `definition_digest`, immutable `design`, `base_sha`, `candidate_sha`,
complete children and direct edges. Require the new decision to be eligible and
its derived blocking IDs to be empty. Do not compare two fresh records' complete
`input_digest` as the graph-change test.

Normalize children by ID, edges by type/target, and label sets by UTF-8 order;
reject duplicate identities instead of silently dropping conflicting rows.
Source/design changes, new/open children or changed blocking prerequisites deny
closure. A new observation timestamp or equivalent row ordering does not. Keep
the original canonical record hashing rules; this clarifies revalidation rather
than introducing a different digest encoding. The future `sf_review_admission`
test must include both a benign fresh observation and a real prerequisite edit.

## Verification

`cargo test --offline -p agalma-conductor --test sweep --test designer` passed
25 cases (22 sweep, 3 CLI). It exercises scripted timeout/retry/restart/scoping
and CLI admission, not live model quality or the complete SF.10 pipeline.

The transport review command is:

```sh
cargo test --offline -p agalma-conductor --test ledger_process --test workspace_process --test taskqueue_process --test execution_process --test sandbox_process live_process_smoke -- --nocapture
```

The first restricted attempt failed because Unix socket binding was denied.
An approved rerun then hit a kernel READY timeout. After rebuilding all five
servers, the review ran each already-compiled smoke case directly and required
one executed case without a skip. Four passed: ledger, taskqueue, execution and
sandbox. Workspace failed to announce READY within 20 seconds. The kernel's
earlier failure did not recur; the reason is not proven. The workspace failure
and missing-binary acceptance path must be addressed before FEZ acceptance.

Final commands, binary/source hashes and results are recorded in
`deepseek-review-receipt.json`. The SF decomposition was rechecked after its
ownership/order amendment and passed; the dependency graph has no cycles.
