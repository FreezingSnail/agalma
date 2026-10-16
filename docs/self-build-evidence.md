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
