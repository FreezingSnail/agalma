# Self-build evidence (SF.9 / `agalma-5fy.9`)

Epic-exit evidence for the SF tranche. It proves the **existing production
self-build path** (SF.1–SF.8) with an isolated scratch clone/state and
deterministic scripted transport, and records the remaining deviations. It does
**not** add production features and is **not** a paid/live inference result.

## Scope and identity

- Bead: `agalma-5fy.9` (SF.9 scratch self-build evidence).
- Repository: Agalma; worktree `/Users/connorfranc/code/agalma/.worktrees/agalma-5fy.9`;
  task branch `task/agalma-5fy.9`.
- Source revision tested: base/HEAD `00c7e6bc99428a71d7f71ee0ae1e3a2661441843`
  ("Merge reviewed SF.6 protected verification"), uncommitted task files added
  below (working tree is otherwise clean).
- Delivered files and their SHA-256 at this revision:
  - `crates/agalma-conductor/tests/sf_self_build.rs` — `48e3296339036b5d7a01f07c2780f23bd56bd146b5d18761e571f8b0510d30fb`
  - `fixtures/self-build/task.md` — `3a6c8417b9e3a2603835aafe091e6d1bc7831acef9bc2a8fac17e1f88b6239b4`
  - `fixtures/self-build/README.md` — `f6f34046301663ce5658bac6410502b5a29af48bd912efe28523dde91cd5024c`
  - `docs/self-build-evidence.md` — this file.
- Environment: macOS (`darwin`), `cargo test --offline --locked`, isolated
  `CARGO_TARGET_DIR=/Users/connorfranc/code/agalma/target/worktree-caches/agalma-5fy.9/target`.
- Persistent evidence root: `/Users/connorfranc/code/agalma/target/test-runs/agalma-5fy.9/`
  (outside the task worktree; retained).

## What the proof establishes (behavioral acceptance)

`crates/agalma-conductor/tests/sf_self_build.rs` drives the **real `agalma work`
production path** over a scratch git origin + isolated `bd` store in scripted
transport (`AGALMA_ACTIVITY_MODE=scripted`, in-process ledger/workspace/taskqueue),
and asserts the frozen epic acceptance:

| Case | What it proves |
|---|---|
| `scripted_self_build_delivers_full_context_and_ships_protected_feature` | The repository-mode task receives its **complete frozen specification** (`Scope`/`Files`/`Contract`/`Invariants`/`Non-goals` + design/notes/directive block/`AGENTS.md` + shared-design context), the rendered builder prompt delivers it, and the feature ships **only after the operator protected baseline independently judges behavior** (a `rustc` runner asserting `answer() == 42`, materialized outside every candidate writable root). Exact candidate/integration identity is asserted. |
| `crash_restart_preserves_frozen_inputs_and_merges_once` | Crash after admission preserves the frozen `TaskPackage/v1`; recovery reuses it (never re-resolves a changed environment) and completes on its pins; a second conductor run cannot merge twice. |
| `out_of_scope_candidate_parks_and_never_ships` | A candidate changing an **outside-scope** path is refused by the protected guard, retries to a park, and the origin never advances. |
| `cancelled_task_never_ships` | Cancel + resume + recovery settles as parked/failed with no effects, no receipts, and an unchanged origin; the backlog issue is `blocked`. |
| `flash_builder_selection_is_refused_when_paid_gate_closed` | An explicit **paid Flash** builder selection is refused before any claim (never silently substituted) when the credential/live gate is closed; no execution is created. |
| `self_build_invariants_hold_over_generated_operation_sequences` | Deterministic generated operation sequences (seed `0x5F9_B007`, 48 cases) exercise frozen identity, the scope ship predicate (incl. invalid/boundary paths, renames, nested `.git`), and builder-only routing. |

### Route and allowance receipts (deterministic run)

- Builder route: the **builder-only selector** (`--builder-model <free model>`)
  is pinned for the builder role; the verifier keeps the independently resolved
  legacy global route (`--model <other free model>`). Builder ≠ verifier pins.
- **Flash is the intended paid builder target but is not routed in this run.**
  With the credential/`AGALMA_ALLOW_PAID` gate closed, `--builder-model
  opencode-go/deepseek-v4.1-flash` is *refused* (fail-closed, no substitution),
  and the routing surface selects Flash only when the paid gate is open.
- Allowance/usage: the fixture carries `budget_usd: 0.00`; scripted transport
  records **zero usage**. No paid request is admitted and none is reserved.

### Identity from the retained run (`20261015-sf9-focused-03`)

- Execution `exec:sf9spec-i3x:1`; frozen `package_digest`
  `sha256:32e9c6a15876db45d8ea2e50414bcefc44266f3d923784c935645491ece4cf63`.
- Verified/integrated `candidate_sha` `867fdebac3fcb96de271a1a0922a1b772f16220c`
  == origin `main` HEAD; integration tag `m1/exec-sf9spec-i3x-1` == the candidate.
- Oracle digest `sha256:10f481a33304e16ce6fbe27d69659c26c68e492e206c1f9e0e0af84298d1423d`.
- Origin history: `seed`, `bd init: initialize beads issue tracking`, `candidate`
  (exactly one candidate commit).

## Known limitations

- Everything here is **scripted transport**: the scripted builder is a
  deterministic stand-in for the builder turn. This is **not** a vendor/model
  performance measurement and must not be described as a Flash inference win.
- **Paid/live Flash evidence is skipped** (credential/live gate closed; no
  spending authority). The gated live command is the same test driven with
  `AGALMA_ACTIVITY_MODE=live` and a certified paid profile under explicit
  authorization; it was not run.
- SF.9 owns no production code; it composes SF.1–SF.8 surfaces. A defect below
  is a sibling/regression deviation, not an SF.9 change.

## Commands, exit status, and raw logs

All attempts share one environment; each is retained under the persistent
evidence root with its own raw log and scratch.

- Worktree: `/Users/connorfranc/code/agalma/.worktrees/agalma-5fy.9` (branch `task/agalma-5fy.9`).
- Source revision: `00c7e6bc99428a71d7f71ee0ae1e3a2661441843` for **every** attempt
  (no commit was made; the only change between focused attempts was the pending,
  untracked `crates/agalma-conductor/tests/sf_self_build.rs`).
- Isolated cache: `CARGO_TARGET_DIR=/Users/connorfranc/code/agalma/target/worktree-caches/agalma-5fy.9/target`.
- Focused attempts also set `AGALMA_TEST_RUN_ROOT=<run>/scratch` and
  `AGALMA_TEST_RETAIN=1` (retained origins/state/protected roots/prompts/artifacts).
- Evidence root: `/Users/connorfranc/code/agalma/target/test-runs/agalma-5fy.9/`.

### Retained attempts

| Run id | Command | Exit | Result / stopping point | Raw log |
|---|---|---|---|---|
| `20261015-sf9-focused-01` | focused (below) | 101 | FAILED 2 passed / 4 failed, 18.78s | `20261015-sf9-focused-01/focused.log` |
| `20261015-sf9-focused-02` | focused | 101 | FAILED 4 passed / 2 failed, 19.82s | `20261015-sf9-focused-02/focused.log` |
| `20261015-sf9-focused-03` | focused | 0 | PASS 6 passed / 0 failed, 21.46s | `20261015-sf9-focused-03/focused.log` |
| `20261015-sf9-focused-04` | focused | 0 | PASS 6 passed / 0 failed, 20.97s (superseded by focused-05) | `20261015-sf9-focused-04/focused.log` |
| `20261015-sf9-focused-05` | focused (post-NUL-repair) | 0 | PASS 6 passed / 0 failed, 22.36s | `20261015-sf9-focused-05/focused.log` |
| `20261015-sf9-workspace-01` | `cargo test --offline --locked --workspace` | 101 | FAILED (fail-fast; partial coverage, see below) | `20261015-sf9-workspace-01/workspace.log` |
| `20261015-sf9-conductor-nff-01` | `cargo test --offline --locked -p agalma-conductor --no-fail-fast` | 101 | FAILED (complete enumeration; supplemental) | `20261015-sf9-conductor-nff-01/conductor.log` |

Focused command (run id `RN`, `RN/scratch` under the evidence root):

```sh
cd /Users/connorfranc/code/agalma/.worktrees/agalma-5fy.9
AGALMA_TEST_RUN_ROOT=<evidence-root>/<RN>/scratch AGALMA_TEST_RETAIN=1 \
CARGO_TARGET_DIR=/Users/connorfranc/code/agalma/target/worktree-caches/agalma-5fy.9/target \
cargo test --offline --locked -p agalma-conductor --test sf_self_build -- --nocapture
```

### Focused attempt detail (required)

- `focused-01` — **first iteration of the first focused run; superseded, not a
  defect.** 4 failures were in the new assertions still being brought up
  (`scripted_self_build…:669`, `crash_restart…:732`, `out_of_scope…:777`,
  `cancelled…:839`); the cause was an absolute origin commit count, since
  `bd init` records its own commit. Log:
  `20261015-sf9-focused-01/focused.log`.
- `focused-02` — **second iteration; superseded.** 2 failures remained
  (`scripted_self_build…:671` integration-tag lookup, `cancelled…:848` issue
  status parsing). Log: `20261015-sf9-focused-02/focused.log`.
- `focused-03` — **required focused PASS** (`6 passed; 0 failed`, 21.46s, exit 0)
  at pending `sf_self_build.rs` sha256 `1ccbc96…207161`. Property record:
  `seed=0x5F9_B007 cases=48 allowed=2 denial-cases=30`. Log:
  `20261015-sf9-focused-03/focused.log`.
- `focused-04` — **post-repair focused PASS** (`6 passed; 0 failed`, 20.97s,
  exit 0) at pending `sf_self_build.rs` sha256
  `b74a8d3b91ddbc226a5b7b1d4ae452ef8933e132e10cd741d8cfddd2f03a34d0`. Property
  record: `seed=0x5F9_B007 cases=48 allowed=2 denial-cases=30 boundary-checks=56
  (budget 48)`. **Superseded** by `focused-05`: that revision covered 6 malformed
  classes (it lacked the explicit NUL destination the comment claimed), so its
  boundary total was 6 + 2 + 48 = 56. Log:
  `20261015-sf9-focused-04/focused.log`.
- `focused-05` — **post-NUL-repair focused PASS, authoritative for this delivery**
  (`6 passed; 0 failed`, 22.36s, exit 0) at pending `sf_self_build.rs` sha256
  `48e3296339036b5d7a01f07c2780f23bd56bd146b5d18761e571f8b0510d30fb`. The
  malformed-class array now contains the NUL destination (`"\0"`) the comment
  describes: **7 malformed classes** + 2 rename-source classes + 48 generated
  checks = **57** boundary checks. Property record: `seed=0x5F9_B007 cases=48
  allowed=2 denial-cases=30 boundary-checks=57 (budget 48)`. Log:
  `20261015-sf9-focused-05/focused.log`.

### Whole workspace attempt (`20261015-sf9-workspace-01`) — required, once — FAIL

```sh
cargo test --offline --locked --workspace
```

- Result: **exit 101**. `cargo test` failed fast, so this is a **partial**
  (not full) workspace coverage: it executed 4 test targets and then stopped.
  No passing whole-workspace result can be inferred.
  - `agalma-conductor` lib unit tests: 177 passed / 0 failed.
  - `agalma-conductor` `main`: 0 passed / 0 failed.
  - `architecture`: 9 passed / 0 failed.
  - `cargo_home`: **1 passed / 1 failed** — stopping point.
- Raw log: `20261015-sf9-workspace-01/workspace.log`.

**The one reproducible failure:**

```
crates/agalma-conductor/tests/cargo_home.rs::sandboxed_acceptance_resolves_cargo_offline

agalma: verify attempt 1 refused: protected inputs refused: activity known failure:
known failure: {"code":"InvalidRecord","format":"FailureDetail/v1",
"message":"frozen protected entry is malformed: expected value at line 1 column 1",
"subject":"scope/oracle"}
```

- Where: `cargo_home.rs:277`, asserting the offline acceptance is green.
- Cause (bounded analysis; no repair attempted): SF.6 (`agalma-5fy.6`,
  commit `c77cbe9`, merged in `00c7e6b`) made repository-mode verification
  fail-closed: `VerifyActivity::guard_candidate` parses the frozen `scope.oracle`
  as a `ProtectedEntry/v1`. `cargo_home.rs` predates SF.6 and builds a package
  with a non-entry oracle, so the guard correctly refuses and the execution
  parks. It is **unchanged by SF.9** and is not an SF.9 change.
- This file is owned by another task; per the handoff it was **not repaired**.

### Supplemental enumeration (`20261015-sf9-conductor-nff-01`) — not authoritative

Run before the stop instruction to inventory deviations (kept only as retained
raw evidence; **not** part of the required acceptance and **not** to be repeated):

```sh
cargo test --offline --locked -p agalma-conductor --no-fail-fast
```

- Result: **exit 101**; a **complete** enumeration (all conductor test binaries
  ran; 27 binaries reported `FAILED`; 135 individual `… FAILED` lines). Raw log:
  `20261015-sf9-conductor-nff-01/conductor.log`.
- Confirms the same SF.6 repo-mode class in `sf_builder_route.rs`
  (`production_pins_scoped_builder_and_independent_verifier`,
  `restart_after_environment_change_preserves_role_pins`: "no readable operator
  protected inputs … refusing repository work") alongside `cargo_home.rs`.
- Many fast-failing transport/process targets (`execution_process`,
  `ledger_process`, `harness_process`, `taskqueue_process`, `workspace_process`,
  `rebind`, `sandbox_process`) are consistent with the documented prerequisite
  gap: neither the required workspace command nor this enumeration ran the
  full-scope prerequisite `cargo build --offline --workspace --bins`
  (docs/development-workflow.md). Treat those as **unverified prerequisites**,
  not confirmed regressions. No positive result is inferred from them.

## Review focus

1. The required whole-workspace acceptance is **not green at this revision**,
   independent of SF.9. The concrete, minimal, out-of-ownership repair is to make
  the affected sibling fixtures (starting with `cargo_home.rs`, then
  `sf_builder_route.rs`) supply a valid `ProtectedInputs/v1` entry / frozen
   `ProtectedEntry` oracle, or to re-scope repository-mode verification for their
   fixture intent. Sol decides ownership and authorization.
2. Confirm the Flash builder target should stay *refused-not-substituted* under a
   closed paid gate, and whether a separately authorized live run is wanted.

### Integrated acceptance after SF.12–SF.14 repairs (2026-10-15)

The later integrated runs supersede the earlier stopping point above as the
current acceptance status, while retaining each earlier run as historical
evidence:

- `20261015-sf9-integrated-full-01`: build passed. Full test attempt was
  interrupted by sandbox socket denials; the escalated retry passed those
  provider tests but exposed an overlong Unix socket path. The shortened-root
  retry then exposed the still-unrepaired SF.6 fixture incompatibility.
- `20261015-sf9-integrated-full-02`: build passed. After SF.12/SF.13 were
  integrated, the workspace test reached `execution_process` and failed
  `shadow_parity_in_process_vs_process` because cancellation timestamps differed
  in parity snapshots. That finding was isolated to SF.14.
- `20261015-sf9-integrated-full-03`: binary build passed. The workspace test
  reached the `fencing` target; two of four tests failed with
  `InvalidRecord: frozen protected entry is malformed: expected value at line 1
  column 1`. The other two fencing tests passed. A focused rerun reproduced the
  same two failures; its third test failed only because the non-escalated run
  could not bind a local Unix socket. The two malformed-oracle failures are
  outside SF.9 ownership and are tracked by child `agalma-5fy.15`, which owns
  only `crates/agalma-conductor/tests/fencing.rs`.

For full-03, the task worktree source revision is
`4818c9ee5b910175e87ee10813ede836afbb33da`. Exact commands and source hashes are
in `20261015-sf9-integrated-full-03/source.env`; the raw build and test outputs
are `20261015-sf9-integrated-full-03/build.log` and `test.log`.

**Current acceptance remains incomplete.** Re-run the required full workspace
acceptance after the bounded SF.15 repair is reviewed and integrated. Do not
claim the workspace suite passed from these partial runs.

### Integrated acceptance after SF.15 repair (2026-10-15)

`20261015-sf9-integrated-full-04` includes SF.15 commit
`730577126449b3e9605dc36fee22553c5327bf6b` in the SF.9 task branch. The
workspace binary build passed. The required full workspace test then stopped at
`agalma-conductor --test gc`: one of three cases passed and two failed because
their repository-mode fixtures lack readable `protected-inputs.json` operator
inputs (`ScopeDenied`, no file at the retained scratch state paths). The repaired
fencing target passed 7/7; digest passed 8/8; builder route passed 13/13; and
execution process passed 7/7. This full run is still partial, not a workspace
pass. Raw build/test logs, statuses, and source identity are retained under
`20261015-sf9-integrated-full-04/`.

The out-of-ownership `gc.rs` fixture mismatch is tracked by the next bounded
child. Re-run full workspace acceptance after that repair is reviewed and
integrated; do not repeat the suite before that change.

### Final integrated acceptance — SF.9 exit (2026-10-15)

The workspace suite is **green**. The remaining failures were all one defect
class: SF.6 (`c77cbe9`) made repository verification fail-closed by parsing the
frozen `scope.oracle` as a `ProtectedEntry/v1`, but earlier acceptance fixtures
still seeded opaque oracle bytes or unseeded operator inputs. They were repaired
by bounded children, integrated into `task/agalma-5fy.9`, and re-verified:

- `agalma-5fy.14` — cancellation-settlement timestamp normalization in
  `execution_process.rs` shadow parity.
- `agalma-5fy.16` — SF.6 operator protected inputs seeded in `gc.rs`.
- `agalma-5fy.17` — SF.6 inputs/oracle seeded in `reap.rs`, `rebind.rs`,
  `sf_cancel.rs`, `verifier.rs`, `watchdog.rs`, `work.rs`, `sf_builder_prompt.rs`,
  `xcrun.rs` and `tests/common/mod.rs`; plus the incidental SF.6 regression in
  `phases.rs` that dropped the scripted `prompts/verifier@N.md` artifact
  (restored to its pre-SF.6 unconditional placement).
- `agalma-5fy.15` — fencing protected-oracle fixture.

Run `20261015-sf9-integrated-full-10`:
- Source revision `88fffec` (SF.9 deliverables present in the working tree).
- `cargo build --offline --workspace --bins` → exit 0.
- `AGALMA_TEST_RUN_ROOT=/Users/connorfranc/code/agalma/target/test-runs
  AGALMA_TEST_RETAIN=1 cargo test --offline --workspace` → **exit 0**, 103 test
  targets `ok`, 0 `FAILED`, 0 ignored-required cases. Raw `build.log`,
  `test.log`, `build.status`, `test.status`, and `source.env` are retained under
  `20261015-sf9-integrated-full-10/`.
- Enumerations retained: `20261015-sf9-diag-06` (repo-mode suspects),
  `20261015-sf9-diag-09` (complete conductor `--no-fail-fast`).

Note on paths: earlier sections reference the worktree
`/Users/connorfranc/code/agalma/.worktrees/agalma-5fy.9`; the live, correct
worktree is `/Users/connorfranc/code/agalma/target/worktrees/agalma-5fy.9`. The
historical attempts are unchanged.

# SF.10 fresh goal-to-closure pipeline proof (`agalma-5fy.10`)

Appended 2026-10-16 by `opencode-go/deepseek-v4.1-flash`. This section is
separate from and does not rewrite any SF.9 section above; those historical
receipts/identities are unchanged.

## Scope and identity

- Status: **blocked** on production repair `agalma-5fy.18`, whose execution
  approval gate is `agalma-my9`. SF.10 acceptance is not satisfied. The dirty
  task worktree and redirected cache are retained; no changes are committed.
- Bead: `agalma-5fy.10` (SF.10). Human-requested bounded verification
  experiment (scripted local verification; **not** live-provider acceptance,
  promotion, or SF epic completion).
- Repository: Agalma; worktree
  `/Users/connorfranc/code/agalma/target/worktrees/agalma-5fy.10`; task branch
  `task/agalma-5fy.10`; base/HEAD `0ba04eee0a5de596a81d14742c8d64555d8b7ba2`.
- Working files and their SHA-256 for the superseded initial run
  (`20261016-sf10-focused-01/source.env`; untracked files included):
  - `crates/agalma-conductor/tests/sf_pipeline_proof.rs` (new) —
    `5977abd0ff60cbbf26c6cb5914cdbdf3e3e51ff98de457462e59a0abfbcfd87e`
  - `fixtures/self-build/pipeline-goal.md` (new) —
    `00cf6cbaf766559e309b85010c6a7710ae98e22fd5523c9e6316fb605e822b38`
  - `docs/self-build-evidence.md` (historical document before this SF.10 append) —
    `0291df3e21c7f943c6fcb8ef98f2d3e9f21716e4db07263c2d4181466b7cebaa`
- Frozen pre-run source identity (HEAD, `git status --porcelain`, `git diff`,
  SHA-256, and `git diff --no-index` byte copies of the untracked files) is under
  `20261016-sf10-focused-01/source.env` and `20261016-sf10-focused-01/source-copies/`.
  `build-inputs.txt` records only HEAD; the SHA-256 + no-index copies supplement
  it honestly.
- The failing required-assertion run freezes its own sources in
  `20261016-sf10-baseline-defect-01/source.env` and `source-copies/`. Its test
  source SHA-256 is
  `c1ff81962cdaefa917e49ffd7fa94445af9924816c6614535d8041715e81ef49`;
  the goal fixture is unchanged.
- Environment: macOS (`darwin`), `cargo test --offline --locked`, isolated
  `CARGO_TARGET_DIR=/Users/connorfranc/code/agalma/target/worktree-caches/agalma-5fy.10/target`,
  `CARGO_INCREMENTAL=0` (runner default).
- Persistent evidence root (outside the worktree):
  `/Users/connorfranc/code/agalma/target/test-runs/agalma-5fy.10/`.

## Experiment stages and observations

`crates/agalma-conductor/tests/sf_pipeline_proof.rs` drives the **real
production goal-to-closure control paths** over a fresh bounded scratch
source-feature goal (`fixtures/self-build/pipeline-goal.md`) in an isolated
scratch git origin + scratch `bd` store. Only the *model/harness responses* are
scripted through the established seams: the `ConciergeSession`, `DesignerSession`
and `ReviewerSession` injected traits. The output-contract parse, planner-route
decision, `bd` epic/design verification, CP.1 decomposition gate, native
human-gate reconciliation, `TaskPackage/v1` admission, protected behavioral
verification, integration, prerequisite-aware review admission, closure recheck,
and actual `bd` epic closure are all real.

| Case | Intended check and observed coverage |
|---|---|
| `fresh_goal_reaches_staged_children_then_execution_review_and_epic_closure` | A fresh goal becomes a designed epic; the designer authors 8 open, approval-held `staged` children each pinned by exactly one well-formed **unresolved** native human gate; approval does not release them (no ready work, no execution); the human resolves each gate; every released child executes through `agalma work` (scripted activity mode), the immutable package + operator **protected baseline** independently judge behavior, and the candidate integrates exactly once. Then, in the same lineage, an open blocking prerequisite waits with **zero reviewer calls** and no drift hold; the completed prerequisite admits **exactly one** review; the epic is **actually closed** in `bd`. Repeat runs add no duplicate integration and no duplicate review. It also **requires** that the admitted review baseline precede the integrated candidate and that the reviewer's diff cover the actual first `src/lib.rs` feature change (`answer() 0 -> 42`) — the assertion that currently fails on `main` (see the missing-baseline defect below). |
| `invalid_decomposition_cannot_close_the_epic` | A rule-violating decomposition is rejected, its children are `decomp-rejected`, exactly one `decomp-fix` rework task is filed, no child is approved, and the epic stays open. |
| `missing_human_release_leaves_children_approval_held_and_the_epic_open` | Approved-but-unreleased children stay open and approval-held by their native gates; no execution is created, the origin never advances, and the epic never silently closes. |
| `timeout_after_authoring_reconciles_the_committed_decomposition` | A designer that hits the turn bound **after authoring** still reconciles its committed decomposition in the same pass (both `timed_out` and `approved`); the epic stays open. |
| `open_prerequisite_waits_with_zero_reviewer_calls_and_no_new_drift_hold` | An open blocking prerequisite yields `WaitingPrerequisite` with 0 reviewer dispatches, no `review-drift` label, no `drift-fix` task, and no global dispatch hold. |
| `drift_withholds_epic_closure` | A `REVIEW: DRIFT` verdict withholds closure, applies the `review-drift` hold, and files one `drift-fix` rework task. |
| `graph_or_design_edit_withholds_stale_closure` | A blocking-edge or design edit between reviewer dispatch and approval records a stale admission, denies closure, and leaves the epic open. |
| `pipeline_gate_invariants_hold_over_generated_operation_sequences` | Despite its name, this property generates static child sets and defect intents, not operation sequences. With seed `0x5F10_9001_CAFE_0007` and 240 generated-input cases, it checks the composed decomposition-gate verdict against an independent model, rule-level details (`count-budget`, `schema-section`, `intake-block`), and approval association classifications (`Missing`/`Unresolved`/`Malformed`). Both accepted and rejected inputs are required. Human resolution is exercised by the concrete happy-path case. |

### Superseded run `20261016-sf10-focused-01` (insufficient assertions — NOT a full proof)

> Superseded by the required-assertion run below. Its 8/8 result reflected
> assertions that accepted an **empty** reviewer diff; it did **not** prove
> independent review of the integrated feature, so it is retained as
> superseded evidence only, never as SF.10 acceptance.

- `status=passed`, `passed=8 failed=0 ignored=0`, `peak_concurrency=1`, exit `0`
  (`jobs/0.status` = `0`); total exec 177.25s.
- Retained happy-path fixture `sf-pipeline-full-*`: origin history is
  `seed`, `bd init: initialize beads issue tracking`, `candidate
  candidate/exec-sfafull-<id>.1-1` (one verified candidate commit);
  8 scope pins; 8 materialized protected `runner.sh` judges; the review attempt
  snapshot (`admission.json`, `definition.json`, `design.txt`, `diff.patch`,
  `prompt.md`) and `approved.json`; review admission
  `decision=eligible`, `children=8`, `blocking_ids=[]`.
- Route/allowance: **no live or paid inference**. The boot sweep/review paid
  planner gate stays unavailable to the work CLI (only the *builder-scoped* free
  selector is passed; the legacy global `--model` is never set), so no live
  designer/reviewer session can slip through. A paid Flash builder selection is
  refused, never substituted. Scripted transport records zero usage.

### Missing review-baseline defect (production) — FAILED required experiment (`20261016-sf10-baseline-defect-01`)

The happy trace now carries a **required** assertion: the admitted review
baseline must precede the integrated candidate and the reviewer's diff must cover
the actual first `src/lib.rs` feature change (seed `answer() == 0` -> candidate
`answer() == 42`). It does **not** hand-set `review-start`, add a pre-landing
commit, or vary sibling content; the production call ordering is the subject.

On current `main` (`0ba04ee`) this assertion **fails**, exposing a production
defect: `work.rs` calls `review::record_start_on_land` **after** `execute_task`
returns `Terminal::Done` (after the first child integrates), and
`review.rs` `record_start_on_land` samples the **current** `HEAD`. The first
feature change is therefore excluded from the epic review baseline.

- Run: `target/test-runs/agalma-5fy.10/20261016-sf10-baseline-defect-01/`
  (`summary.json` `status=failed`, `passed=0 failed=1`, `jobs/0.status` = `101`).
- Exact failure (`sf_pipeline_proof.rs:1111`):
  `assertion left != right failed: the admitted review baseline must precede the
  integrated candidate; production sampled it after the first feature landed,
  excluding that change from the review` — `base_sha == candidate_sha`.
- Retained fresh lineage (`fixtures/sf-pipeline-full-*`, epic `sfafull-kxp`):
  - origin: `seed 8204864` -> `bd init 5164a7a` -> `candidate 14c9223`
    (`candidate candidate/exec-sfafull-kxp.1-1`).
  - admitted `base_sha = candidate_sha = 14c9223…`; reviewer `diff.patch` = **0
    bytes**.
  - the excluded change (`bd-init 5164a7a` -> `candidate 14c9223`, `src/lib.rs`)
    is the actual feature (`answer() 0 -> 42`).
- Minimal diagnostic + raw artifacts: `defect-diagnostic.md`,
  `raw-admission-snapshot.json`, `raw-reviewer-diff.patch`, `raw-origin-log.txt`
  under that run dir.
- This is a **production defect outside SF.10 ownership** (production `work.rs` /
  `review.rs`). Per the SF.10 stopping condition it is **reported, not repaired**:
  no production source change, commit, closure, or sibling dispatch was made.
  Repair bead `agalma-5fy.18` directly blocks SF.10; its native human execution
  approval gate `agalma-my9` remains open. The repair has not been dispatched.

## Known limitations

- Everything here is **scripted transport**: the model/harness responses are
  deterministic stand-ins. This is **not** a vendor/model performance
  measurement and must not be described as a Flash inference win.
- **The empty reviewer diff is a production defect, not a fixture property**
  (see the missing-baseline defect above). Because production records the review
  baseline after the first child integrates, the first feature change is excluded
  from the review; a non-empty diff would only appear for later sibling changes
  and would still omit the first change. Until the defect is repaired, the SF.10
  happy trace fails its required assertion and SF.10 is **not** verified.
- The stale graph/design negative cases use a distinct `landed_epic` setup
  (a pre-landed closed `staged` child + recorded baseline), separate from the
  fresh-goal lineage; they are disclosed as such and exercise only the review
  recheck boundary.
- This proof composes existing SF/SF.11 production surfaces; it adds no
  production code. A defect found elsewhere is a sibling deviation, not an SF.10
  change.

## Commands, exit status, and raw logs

Initial focused run (unified runner; `RUN_ID` set explicitly) — **superseded**
by the required-assertion run (its assertions accepted an empty reviewer diff):

```sh
cd /Users/connorfranc/code/agalma/target/worktrees/agalma-5fy.10
BEAD=agalma-5fy.10 RUN_ID=20261016-sf10-focused-01 \
RUN_ROOT=/Users/connorfranc/code/agalma/target/test-runs \
CARGO_TARGET_DIR=/Users/connorfranc/code/agalma/target/worktree-caches/agalma-5fy.10/target \
AGALMA_TEST_RUN_ROOT=/Users/connorfranc/code/agalma/target/test-runs/agalma-5fy.10/20261016-sf10-focused-01/fixtures \
AGALMA_TEST_RETAIN=1 \
make test-focused SELECTION='-p agalma-conductor --test sf_pipeline_proof'
```

- Result: **exit 0**, `8 passed / 0 failed / 0 ignored` (superseded; not a full
  proof).

Required-assertion run (single happy-path filter) — **FAILED**, demonstrating the
production defect:

```sh
cd /Users/connorfranc/code/agalma/target/worktrees/agalma-5fy.10
BEAD=agalma-5fy.10 RUN_ID=20261016-sf10-baseline-defect-01 \
RUN_ROOT=/Users/connorfranc/code/agalma/target/test-runs \
CARGO_TARGET_DIR=/Users/connorfranc/code/agalma/target/worktree-caches/agalma-5fy.10/target \
AGALMA_TEST_RUN_ROOT=/Users/connorfranc/code/agalma/target/test-runs/agalma-5fy.10/20261016-sf10-baseline-defect-01/fixtures \
AGALMA_TEST_RETAIN=1 \
make test-focused SELECTION='-p agalma-conductor --test sf_pipeline_proof fresh_goal_reaches_staged_children_then_execution_review_and_epic_closure'
```

- Result: **exit 2 / `status=failed`**, `passed=0 failed=1` (`jobs/0.status` = 101),
  135.43s. Raw log `jobs/00-agalma-conductor::sf_pipeline_proof.log`; minimal
  diagnostic `defect-diagnostic.md`; raw `raw-admission-snapshot.json`,
  `raw-reviewer-diff.patch`, `raw-origin-log.txt`; pre-run `source.env` +
  `source-copies/`.
- Superseded initial run's raw log/status/env/fixtures:
  `target/test-runs/agalma-5fy.10/20261016-sf10-focused-01/`
  (`summary.json`, `summary.txt`, `run.env`,
  `plan.txt`, `build-inputs.txt`, `jobs/00-agalma-conductor::sf_pipeline_proof.log`,
  `jobs/0.status`, `source.env`, `source-copies/`, `fixtures/`, `build/`).

For a rerun, choose a new `RUN_ID` and matching absolute fixture path; preserve
these historical run directories. The commands above record the original runs.

### Development iteration (`20261016-sf10-dev-01`) — non-authoritative

- A raw-binary dev iteration was used while bringing the test up (superseded by
  the authoritative runner run above). Its preserved fixtures are under
  `target/test-runs/agalma-5fy.10/20261016-sf10-dev-01/fixtures/`; the exact
  command, observed `8 passed / 0 failed`, exit `0` (231.43s), and the honest
  note that **pre-run source identity was not captured** are recorded in
  `20261016-sf10-dev-01/dev-invocation.md`. That identity is not reconstructed
  from later-edited sources; use `20261016-sf10-focused-01/source.env`.

## Review focus

1. **Production defect (SF.10 result):** epic review baseline excludes the first
   integrated feature change. `work.rs` records `review::record_start_on_land`
   after the child lands and `review.rs` samples current `HEAD`, so
   `base_sha == candidate_sha` and the reviewer `diff.patch` is empty. SF.10's
   required assertion fails on `main` (`0ba04ee`); repair `agalma-5fy.18` blocks
   SF.10. No production change was made here.
2. SF.11's waiting -> prerequisite completion -> exactly one review/closure is
   exercised **inside the same fresh goal-to-execution lineage** (the happy
   path), in addition to the separate `landed_epic` negative fixtures. Those
   other cases pass; they use a distinct `landed_epic` baseline setup (disclosed)
   and do not mask the defect.
3. Superseded run `20261016-sf10-focused-01` (8/8) accepted an empty reviewer
   diff and is retained as superseded evidence only; it is not SF.10 proof.
4. This is scripted verification only. No live/paid acceptance, promotion, or SF
   epic closure is claimed.

## SF.10 post-repair verification (2026-10-17)

The preceding defect finding and failed assertion describe the pre-fix revision
`0ba04eee0a5de596a81d14742c8d64555d8b7ba2`. The reviewed SF.18 repair was
integrated as `c0f4eea42d6e41e3e359a4bc0b75f57ac3fa9d6f`; the retained SF.10
checks now pass on that source. The historical failure and superseded empty-diff
run above remain unchanged. Status and gate statements in the earlier SF.10
appendix are also historical: SF.18 has been reviewed, merged, and its execution
gate closed.

- Happy-path regression: `20261017-sf10-post-fix-happy-01`, passed 1/1,
  compile 4,294 ms, execution 184,014 ms. Its reviewer admission uses the
  pre-integration base and its diff contains the first `src/lib.rs` feature
  change.
- Required SF.10 target: `20261017-sf10-post-fix-required-01`, passed 8/8,
  compile 382 ms, execution 223,035 ms, 0 failed/ignored.
- Both raw run records, exact commands/environment, logs and fixtures are under
  `/Users/connorfranc/code/agalma/target/test-runs/agalma-5fy.10/` in the
  corresponding run directories. Pre-run source identity is recorded in
  `source-identity-post-sf18.txt` and `source-identity-post-sf18-full.txt`;
  both identify HEAD `c0f4eea` and the same test/goal source hashes. The SF.10
  working changes remain in `task/agalma-5fy.10` for Sol review.
- This establishes the scripted local SF.10 acceptance against the reviewed
  fix. It does not establish live-provider behavior, promotion, or SF epic
  closure.
