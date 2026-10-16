# Development workflow

Human-directed workflow amendment agreed on 2026-10-13. `AGENTS.md` makes these
rules applicable to repository sessions. Historical design receipts retain their
original snapshots and hashes; this amendment does not reseal those inputs.

## Responsibilities

| Work | Sol | GPT-6 Luna / DeepSeek Flash through OpenCode |
|---|---|---|
| Architecture | Discuss tradeoffs; define boundaries and contracts | Implement the accepted design |
| Planning | Maintain designs and bounded beads with clear acceptance | Execute the assigned bead |
| Code | Specify changes and diagnose review findings | Write source, tests, scripts, build configuration, and repairs |
| Verification | Select scope and assess results | Run agreed checks and provide logs and results |
| Review | Compare the change with design and acceptance | Repair findings within the agreed scope |

Use `agents/sol-architect.md` as Sol's persona. `AGENTS.md` instructs Sol sessions
to load it automatically. A session can explicitly invoke it by saying: "Use the
Sol architect persona in agents/sol-architect.md to plan/review <task>." This is a
repository prompt, not a newly registered runtime model route.

## Working cycle

1. Discuss the outcome, architecture, and material tradeoffs with the user.
2. Sol records the accepted design and prepares one bounded implementation bead.
3. Luna or Flash implements through OpenCode and runs the agreed checks.
4. Sol reviews the diff and evidence. Concrete findings go back to the implementer.
5. After review approval, commit only the task's accepted files or hunks, close
   when acceptance is satisfied, and leave one concise handoff including the
   commit ID. Preserve unrelated and unreviewed edits; report an ownership overlap
   if it prevents a clean task commit. Sol may commit reviewed documentation.

Each implementation handoff needs the goal and relevant context, owned files,
contracts and dependencies, observable before/after behavior, acceptance scope,
and a stopping condition. Tell implementers they share the repository: preserve
others' edits and coordinate overlapping ownership. Follow the existing bead
schema and authorization rules. Model selection alone does not release work or
authorize spending. If the permitted route is unavailable, keep the handoff ready
and report that condition instead of changing implementation ownership.

Implementation and repairs use a named Git worktree and task branch, retained
across attempts. Record the bead, worktree path, branch, and base commit before
editing. Do not implement in the shared primary checkout or a disposable temp
directory. Preserve the worktree through review and repair. Once the reviewed
task changes are committed and merged into `main`, remove its clean task worktree
as part of delivery without another confirmation. Confirm test sources and
fixtures are delivered and preserve all run evidence at a persistent artifact
root outside the worktree before removal. Preserve dirty or unmerged worktrees,
report the remaining work, and never force removal or discard changes. Retain
task branches unless their deletion is separately requested.

The session-resume ownership policy from the retrospective's item 1 is deferred.
Do not introduce a new closed-bead resume policy as part of this amendment.

## Reusable build and verification entry point

Use the root [Makefile](../Makefile) for routine builds, scoped checks, and
build-artifact hygiene:

```sh
make             # same as make all / make build
make release     # all workspace targets in the release profile
make test-focused SELECTION='-p <crate> --test <target> [filter]'
make test-peers  # five local process smokes on the default Connect wire
make test-full   # workspace tests (excludes ignored live-provider checks)
make report      # build-artifact sizes, no changes
make sweep       # classify worktree caches; sweep-apply reclaims them
make help        # targets, scope, and overrides
```

The debug build runs `cargo build --offline --locked --workspace --all-targets`.
The release build adds `--release`. These commands compile all product workspace
libraries, binaries, examples, tests, and benches; they do not execute tests,
launch services, or build standalone spikes and fixture projects. New workspace
members are included through Cargo without editing a component list in Makefile.

Normal outputs and incremental caches remain in `target/debug/` and
`target/release/`. Override `CARGO` to select the Cargo executable,
`CARGO_FLAGS` to change the default `--offline --locked` options, or
`CARGO_TARGET_DIR` to reuse an established alternate cache. The Makefile exports
`CARGO_INCREMENTAL=0` by default: incremental IR is the largest single consumer
inside a cache and cannot be shared between caches. Set `CARGO_INCREMENTAL=1` for
fast local iteration. Download and dependency-change authority still follows the
repository rules. Coordinate shared-cache builds with sessions running process
tests against server binaries.

Compilation is separate from verification. The verification targets below select
a scope and log evidence; `scripts/test.sh` remains the unified runner.

## Worktree build isolation and artifact hygiene

Implementation worktrees share one repository but must not share one build cache
lockstep. Build inside a worktree with a redirected cache:

```sh
CARGO_TARGET_DIR=target/worktree-caches/<bead>/target make build
```

Do not compile into `<worktree>/target/`. An unredirected build writes a second
full cache inside the worktree, so one bead's dependency graph, native test
executables, and incremental IR were previously stored several times over. The
`[profile.dev] debug = 1` setting in `Cargo.toml` keeps line-level backtraces
while avoiding full-DWARF artifact sizes; the IPC codegen rlib and native test
executables are the largest individual artifacts.

A redirected cache is 0.5-4 GiB and is rebuildable, not evidence. Reclaim it when
the bead's reviewed changes are merged and its worktree is removed:

```sh
make report                        # sizes of every target/ consumer
make sweep                         # classify: keep, orphaned, pending, duplicate
make sweep-apply                   # remove orphaned and merged-and-clean caches
make worktree-remove BEAD=<id>     # guarded single-bead worktree + cache removal
```

`make sweep` never removes a registered, dirty, or unmerged worktree or its
cache, and never touches `target/test-runs`. `make worktree-remove` refuses a
dirty or unmerged worktree and refuses while Cargo is running. `make clean-build`
removes only `target/debug` and `target/release`; `make clean-caches` removes
every redirected cache. Retained evidence under `target/test-runs` is never part
of a hygiene target.

## Unified test entry point

Test selection and concurrency follow [testing-strategy.md](testing-strategy.md).
Each handoff names affected behavior, explicit test targets/modules, prerequisites,
resource conflicts, and the reason for any broader acceptance. Whole-crate baseline
and post-change runs are not routine requirements.

The entry point is `bash scripts/test.sh <scope> [selection]`. It is implemented
(agalma-i15) and supports the `focused`, `peers`, and `full` scopes only. The
Makefile scope targets forward to it, so there is a single implementation rather
than two, and they record raw output and exit status under
`target/test-runs/<bead>/<run-id>/`. Set `BEAD=<id>` to label evidence for a task.

The runner requires `jq` (it validates focused selections against `cargo metadata`
and parses Cargo's machine-readable build output) and `rg` (the `peers` scope uses
it to check the smoke evidence). Both are accepted prerequisites, not optional: a
missing tool fails the scope instead of silently narrowing its checks.

| Scope | Selection and purpose | Limits |
|---|---|---|
| `focused` | Explicit crate and required target or unit-test module selection; verifies changed behavior and affected consumers | Does not establish workspace acceptance or unselected process boundaries |
| `peers` | The four required ledger, workspace, taskqueue, and execution local process smokes | Does not establish paid-provider behavior, all process failure paths, or full conductor acceptance |
| `full` | Workspace tests, including doctests, with necessary binary prerequisites | Does not establish ignored live-provider checks or any other unexecuted acceptance |

Example target interface: `bash scripts/test.sh focused agalma-conductor
--test ledger_process live_process_smoke`, `bash scripts/test.sh peers`, and
`bash scripts/test.sh full`. Selection arguments must be validated, not evaluated
as shell text. No scope prints usage without building or running tests. Help must
show what each scope includes and excludes.

The runner must:

- Print the scope, behavior checked, selection, exact commands, and relevant
  transport settings before execution. Handoffs reference this entry point.
- Stream output while saving raw logs and command exit status under
  `target/test-runs/`. Propagate failures and mark interrupted runs incomplete.
- Build only required prerequisites. Reuse a stable build cache; separate caches
  when concurrent tasks would overwrite required executables, then reuse them.
- Compile the selected targets in a coordinated phase, then run independent
  selected test binaries concurrently with bounded jobs and per-binary threads.
  Conflicting resource groups serialize. See testing-strategy.md for the contract.
- Run the five `peers` smokes against real local binaries on the default Connect
  transport. A missing binary, skipped required case, or zero-case selection must
  fail. Preserve the production READY deadline and explicit qualification checks.
- Execute required cases once per scope. Do not append a full suite to `peers`,
  or run `peers` and then repeat those cases in `full` without a specific reason.
- Keep paid live-provider checks outside these scopes and under their applicable
  explicit authorization. Do not enable paid inference through a profile default.
- Produce a simple outcome with executed, failed, and ignored cases. Raw output
  and status remain authoritative; no custom integrity receipt framework is needed
  for ordinary tests. Existing promotion contracts still require their evidence.

During the transition, use the Makefile target for the chosen scope. Each target
forwards to `scripts/test.sh` (implemented in agalma-i15) and keeps the exact
command line, raw log, and exit status under `target/test-runs/<bead>/<run-id>/`.
The runner reproduces the commands below and additionally compiles selected
targets once, discovers matching required cases, runs compiled binaries through a
bounded pool, and writes a per-target summary:

- Focused: `make test-focused SELECTION='-p <crate> --test <target> [filter]'`.
  The runner validates the selection, compiles exactly the selected targets plus
  declared prerequisites (`--prereq <crate>` / `--prereq-bins`, not part of the
  `SELECTION` Cargo args), and fails before any build on an empty or invalid
  selection. An empty `SELECTION` also fails at the Makefile level.
- Peer acceptance: `make test-peers`, which forwards to `bash scripts/test.sh
  peers`. This builds all workspace binaries, preflights each server executable,
  runs the five required process smokes, and then the retained conductor suite.
  `scripts/verify-peer-transports.sh` is now a compatibility forwarder to the
  same scope, so its callers keep the broader behavior.
- Full: `make test-full`, which forwards to `bash scripts/test.sh full` and runs
  `cargo build --offline --locked --workspace --bins` then `cargo test --offline
  --locked --workspace`. An exit of zero does not prove ignored cases ran.

`bash scripts/test.sh peers --print` and `bash scripts/test.sh full --print`
print the exact commands a scope would run without executing them. This is a
diagnostic for verifying the retained contracts, not a substitute for running a
scope.

The unified runner replaces routine alternatives. The old peer script is retained
only as a compatibility entry that forwards to the `peers` scope and preserves the
acceptance behavior its callers require. Do not create additional ad hoc wrappers
during the transition.

## Investigation, artifacts, and review evidence

Before a costly investigation, state the question, the smallest reproducible
example, the effort limit, and the stopping condition. Capture one useful failure
with its exact command, environment, and output. If the issue becomes a larger
build or environment problem, make it a separate bounded implementation task.
Another retry needs a specific changed condition or diagnostic purpose.

Keep artifacts in established repository directories. Do not repeatedly create
fresh temp checkouts, build caches, or disposable Python programs to repeat the
same logging, counting, hashing, or handoff work. Use native commands for ordinary
inspection; recurring executable logic belongs in a reusable repository tool
implemented by Luna/Flash. Ephemeral data is not a reason to build another tool.

Behavioral changes require property tests with independent invariant assertions,
generated inputs or operation sequences, bounded case counts, and reproducible
seeds or deterministic generation. Include applicable invalid/boundary inputs and
recovery/idempotency properties. Persist failing inputs and retain concrete
regression cases. Reuse existing infrastructure; dependency/framework additions
require an explicit design decision. Documentation-only changes need content review.

Retain evidence for every run under `target/test-runs/<bead-id>/<run-id>/` at a
recorded persistent root: exact commands/environment, source identity, raw logs,
exit status, case budgets/seeds, results, and fixture data. Passing and interrupted
runs are retained too. Use ScratchDir's retention APIs where necessary; never
overwrite prior runs or delete evidence without explicit user direction. Delivery
includes all new test sources, generators, and fixtures; worktree removal requires
confirming those sources and run evidence survive outside the removed worktree.

Run focused checks first. Broaden to full acceptance once when the task requires
it or a concrete integration risk justifies it. After checks pass, repeat only
for relevant changes or unresolved concerns. Documentation-only amendments need
link/content review and whitespace checks, not a Cargo suite.

Keep one handoff with the bead, files changed, commands, results, raw log paths,
remaining uncertainty, and next action. Separate reported evidence from checks
the reviewer ran. Partial results from different source states are not one
coherent full run; a subset of matching source hashes does not prove otherwise.
Preserve historical receipts and the stronger evidence required by protected
evaluation and promotion contracts.

For build reliability, investigate the IPC generator's generated `ipc.fds`
rerun directive as a separate bounded task. Verify any fix through repeated
incremental builds before claiming the trigger is resolved. This amendment does
not establish a root cause or implement that fix.
