# Testing strategy

Requested on 2026-10-14; the runner below was implemented in `agalma-i15`.
`bash scripts/test.sh <scope> [selection]` implements concurrent focused
execution and retains the existing peers/full contracts. Its own bounded
verification tests live in `scripts/tests/` (`bash scripts/tests/run.sh`).

## Select behavior before commands

Each bounded bead or handoff records:

- Changed behavior and the callers, contracts, persistence, or process boundaries
  that could regress.
- Required cases and explicit crate/target selections; for unit tests, the relevant
  module or case filter. Explain why each selection belongs.
- Required server binaries, environment, isolated fixture paths, and any shared
  resource that prevents concurrent execution.
- The chosen scope, concurrency budget, and stopping condition.
- A named implementation worktree/task branch, base commit, and persistent
  artifact root outside any worktree scheduled for removal.
- For required property tests, name the changed invariant, generation/case budget,
  and replay strategy. Use the routine-fix handoff in development-workflow.md for
  small changes; record only applicable prerequisites and resource conflicts.

Start with the regression test for the changed behavior, then affected consumer
tests. A source filename alone is insufficient to decide coverage: changes to a
shared contract can require tests in several consuming crates. Missing coverage
requires a meaningful test in the implementation task.

Scale verification to changed behavior and risk. Focused existing regressions or
table-driven precedence, fallback, and error cases normally suffice for a local
config/path change. Add a meaningful regression where coverage is missing; do not
require a new generator or fixed case quota for every behavioral change.

Require property tests when a named changed invariant warrants generated inputs
or operation sequences, especially recovery/idempotency, concurrency, accounting,
or authorization. Existing properties can satisfy that requirement. Required
properties assert independent invariants, use reproducible seeds or deterministic
generation and bounded case budgets, and preserve failing/minimized inputs.
Reuse existing infrastructure; new test frameworks or dependencies require an
explicit design decision. Frozen acceptance and protected checks are unaffected.

Implementation and repairs reuse a named Git worktree. Save all test sources,
generators, and regression fixtures in the reviewed changes. Retain every run's
logs, command/environment, source identity, results, and applicable replay seeds
and fixture data under
`target/test-runs/<bead-id>/<run-id>/` at the recorded persistent root, including
passing and interrupted runs. Use existing ScratchDir retention APIs as needed.
For routine tasks, use existing runner/session logs and one concise handoff;
do not build custom receipts or duplicate reports. Retain additional fixture data
and replay metadata when required for reproduction or a named acceptance contract.
Evidence must survive worktree removal and is never deleted without explicit user
direction. After reviewed task changes are committed and merged into `main`,
remove the clean task worktree as part of delivery, following the evidence checks
in development-workflow.md. Preserve dirty or unmerged worktrees; no forced removal.
Build that worktree with `CARGO_TARGET_DIR=target/worktree-caches/<bead>/target`
so its cache stays outside the worktree, and reclaim the cache with
`make sweep-apply` (or `make worktree-remove BEAD=<id>`) once the worktree is
gone. `target/test-runs` is retained evidence and is never swept.

Do not routinely run whole-crate or workspace suites before editing. A baseline
is useful only to reproduce a concrete failure or satisfy required paired evidence;
use the same necessary selection. After repair, rerun failed selections and those
affected by the repair. Passing selections need repetition only when their inputs
or prerequisites changed.

A whole-crate run requires a stated coverage reason. Workspace acceptance belongs
to an integration or release gate, once for its coherent source state, or a named
integration risk that focused checks cannot resolve. Frozen acceptance and protected
promotion checks remain mandatory. Narrowing a task's frozen commands requires
the existing amendment authority; do not silently replace them.

## Current commands and an example

Use the unified runner: `bash scripts/test.sh focused <selection>`
(or `make test-focused SELECTION='<selection>'`). A selection is one or more
units, each naming an explicit crate and target/module:

```
-p <crate> (--test <name>|--lib|--bin <name>|--example <name>|--bench <name>) [filter...] [--exact]
```

Multiple units select multiple crates/targets in one invocation, and each unit's
filter applies only to its own target. The runner compiles exactly the selected
targets once, then runs the compiled binaries through a bounded pool. It records
the exact commands, raw logs, exit status, compile and execution durations,
resource groups, and observed peak concurrency under
`target/test-runs/<bead>/<run-id>/`.

Focused-runner controls:

- Isolation markers, per unit, default to conservative serialization:
  `--isolated` (known-isolated; may overlap), `--group <name>` (named exclusive
  resource; the same group serializes), or no marker (unknown; serializes with
  every unit). Two `--isolated` units overlap when `JOBS=2`.
- Prerequisites: `--prereq <crate>` builds that crate's bins; `--prereq-bins`
  builds all workspace bins. They are runner options, not Cargo test args.
- Budget: `JOBS` (default 2, capped by CPUs) and `THREADS` (per-binary case
  threads; default divides the CPU budget). The effective budget is printed.
- Rejections: an empty or invalid focused selection fails before a build; a
  filter matching no required cases fails; an ignored-only required selection
  cannot pass; any failing selection fails the run even when another passes.
- Evidence: `$RUN_ROOT/$BEAD/$RUN_ID/` (`RUN_ROOT` defaults to the primary
  checkout's `target/test-runs`, so it survives worktree removal). Raw logs and
  status are authoritative.
- Prerequisites on PATH: `jq` (focused-selection validation against `cargo
  metadata`, and compile-output parsing) and `rg` (peer-scope smoke-evidence
  checks). A missing tool fails the scope; the runner does not silently fall back
  to a narrower check.

Keep Rust's existing case concurrency unless the selected cases require isolation.

For SF.4 cancellation, retain the required conductor `sf_cancel` acceptance target.
Candidate supporting kernel selections are `exec_api`, `transition`, and `recovery`
because cancellation changes adapter semantics, state transitions, and restart
behavior. Review the actual change before finalizing these selections:

```sh
cargo test --offline --locked -p agalma-kernel --test exec_api --test transition --test recovery
cargo test --offline --locked -p agalma-conductor --test sf_cancel
```

These are a selection example, not executed evidence or an amendment to SF.4's
frozen inputs. They do not establish coverage of every cancellation case. Build
only their required server prerequisites and record them separately.

The available `target/test-runs/agalma-5fy.4/kernel-pre.log` records 58.71 seconds
of compilation followed by unrelated targets such as activation, activity, and
pack integrity. It ends during waiting; it does not establish a complete pass.
Compilation time and test execution time must be reported separately before
claiming a performance improvement.

## Unified runner concurrency contract

The implemented runner (`bash scripts/test.sh <scope> [selection]`, agalma-i15)
follows the contract below. Use Cargo and the native Rust test harness;
introducing a test framework or automatic dependency graph is outside this task.

1. Validate a bounded list of explicit selections. Reject a focused request with
   no target/module selection. Support several crates/targets in one request;
   preserve target-specific filters rather than applying one filter accidentally
   to every target. Never evaluate selections as shell code.
2. Coordinate compilation in the stable Cargo cache. Build only selected test
   executables and declared prerequisites, capturing executable paths from Cargo's
   machine output. Do not run multiple competing Cargo builds against that cache
   as a substitute for parallel test execution. Freeze the build inputs for the
   run; a relevant concurrent edit invalidates completion for the prior selection.
3. Execute the compiled native test binaries through a bounded worker pool.
   Default to at most two simultaneous binaries, capped by available CPUs, with
   per-binary thread budgets whose sum does not exceed the CPU budget. Allow
   explicit job/thread overrides and print the effective budget. Keep child-process
   capacity in mind; process-heavy checks can need a lower budget.
4. Only overlap selections with known isolated resources. Assign named exclusion
   groups for shared ports, fixed fixture paths, global services, or binaries that
   another build might overwrite. Unknown isolation serializes conservatively.
   A per-binary `--test-threads=1` does not prevent interference between binaries.
   Reuse established alternate caches only when necessary for executable safety.
5. Preserve Cargo-provided runtime library paths, working directory, task
   environment, and required compile-time binary prerequisites. Process and crash
   tests must retain their child re-execution semantics. Direct execution must
   reproduce the selected Cargo command's behavior before it is accepted.
6. Discover matching required cases before execution and reject empty required
   selections. Required ignored cases cannot count as coverage. Record selected,
   executed, failed, and ignored cases per target; child harness output must not
   inflate the top-level counts. Preserve case filters and any required `--exact`.
7. Give each target a raw log and exit status under one run directory in
   `target/test-runs/`. Record exact commands, compilation and execution duration,
   resource groups, and the observed peak concurrent binaries. Stream labeled
   progress. Every launched child must be reaped; any failure fails the run and
   interruption leaves an incomplete outcome with cleanup of owned processes.

Keep focused, peers, and full as the only scopes. The delivered runner implements
concurrent focused execution and retains the existing peers/full contracts. Full
retains ordinary Cargo execution; accelerating it is outside this task. The peer
script is retained only as the documented compatibility entry, forwarding to the
`peers` scope and preserving the broader checks for callers that require them.
Paid-provider checks remain separately authorized.

## Acceptance for the implementation

- An empty or invalid focused selection fails before a build; a filter matching
  no required cases fails. Ignored-only required selections cannot pass.
- Two independent selected binaries overlap when jobs=2; an exclusion group
  serializes them. jobs=1 runs the same cases and yields equivalent results.
- Target-specific filters select the intended nonzero cases across multiple
  targets and crates. An unrelated test target is neither built nor executed.
- A failing selection propagates failure even when another selection passes.
  Interruption records incomplete status, reaps children, and preserves logs.
- At least one existing process/crash selection runs through the new focused
  path with its prerequisites and child re-execution intact. Validate environment
  parity against the documented scoped Cargo command.
- Existing peers/full scope requirements in development-workflow.md are retained.
  Required acceptance scopes execute once; do not append a workspace suite to
  focused-runner development by default.
- Report durations and observed concurrency for the same selection in serial
  and concurrent mode. Accept behavioral correctness without inventing a speedup
  threshold; compilation may dominate and process contention may erase savings.
- Save bounded property tests for selection isolation, concurrency/exclusion
  limits, and outcome aggregation using independent invariants over generated
  selections and result sequences. Retain their replay inputs and all run evidence
  at the persistent artifact root; implement and repair in the named task worktree.

Stop after this runner and its guidance satisfy these checks. These acceptance
items are covered by `scripts/tests/run.sh` (property tests in
`scripts/tests/runner-properties.sh`; end-to-end cases in
`scripts/tests/runner-focused.sh`), with evidence under the persistent
`target/test-runs/<bead>/<run-id>/` root. Automatic selection, factory scheduling
changes, a new framework, build-trigger repair, and changes to frozen historical
receipts require separate decisions.
