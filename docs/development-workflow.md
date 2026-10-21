# Development workflow

Human-directed workflow amendment agreed on 2026-10-13. `AGENTS.md` makes these
rules applicable to repository sessions. Historical design receipts retain their
original snapshots and hashes; this amendment does not reseal those inputs.

## Responsibilities

| Work | Sol | DeepSeek Flash through OpenCode |
|---|---|---|
| Architecture | Discuss tradeoffs; define boundaries and contracts | Implement the accepted design |
| Planning | Maintain designs and bounded beads with clear acceptance | Execute the assigned bead |
| Code | Specify changes and diagnose review findings | Write source, tests, scripts, build configuration, and repairs |
| Verification | Select scope and assess results | Run agreed checks and provide logs and results |
| Review | Review high-difficulty work or when explicitly requested | DeepSeek Flash reviews routine work in an independent session; implementer repairs findings |

Use `agents/sol-architect.md` as Sol's persona. `AGENTS.md` instructs Sol sessions
to load it automatically. A session can explicitly invoke it by saying: "Use the
Sol architect persona in agents/sol-architect.md to plan/review <task>." This is a
repository prompt, not a newly registered runtime model route.

## Working cycle

1. Discuss the outcome, architecture, and material tradeoffs with the user.
2. Sol records the accepted design and prepares one bounded implementation bead.
3. `opencode-go/deepseek-v4.1-flash` implements through OpenCode and runs the
   agreed checks. Do not dispatch Luna through OpenCode.
4. DeepSeek Flash reviews routine work in an independent session; Sol reviews
   high-difficulty work or when explicitly requested. The assigned reviewer
   returns one consolidated blocking-findings list against accepted scope.
5. After review approval, the same Flash implementer commits only the task's
   accepted files or hunks, performs the authorized merge and clean worktree/cache
   removal, and closes the bead when acceptance is satisfied. Leave one concise
   handoff including the
   commit ID. Preserve unrelated and unreviewed edits; report an ownership overlap
   if it prevents a clean task commit. Sol may commit reviewed documentation.

### Routine fixes

Use one bead and a short handoff naming outcome, owned files, acceptance checks,
scope/effort limits, and stopping condition. An explicit small-fix request supplies
its scope; discuss only material new decisions. Select an advertised implementation
agent explicitly and dispatch directly with Flash. Do not introduce a coordinator,
child agents, or decomposition for a single bounded fix. Use a separate Flash
session for a short review of the diff and existing evidence.

Implement the requested behavior using existing tools and fixtures. New harnesses,
policy generators, and independent prerequisites require separate scope agreement;
do not bundle them into a repair. Updating affected existing fixtures is ordinary
implementation, not a reason to build new tooling.

At about 15 minutes of elapsed effort or after the first repair round, whichever
comes first, reassess scope and identify the cause of delay and a bounded next
step. Carry elapsed effort across resumes. Stop expansion for scope agreement when
needed; the checkpoint does not waive correctness or authorize additional spending.

Review findings must identify concrete acceptance failures, regressions, or contract
violations and the smallest correction. Preferences and unrelated improvements are
nonblocking follow-ups. After approval, continue delivery in the same implementer
session without another planning phase or repeated passing checks unless relevant
inputs changed. Report a missing delivery permission or route rather than silently
transferring ownership. Documentation/instruction edits remain within Sol's remit.

Each implementation handoff needs the goal and relevant context, owned files,
contracts and dependencies, observable before/after behavior, acceptance scope,
and a stopping condition. Tell implementers they share the repository: preserve
others' edits and coordinate overlapping ownership. Follow the existing bead
schema and authorization rules. Model selection alone does not release work or
authorize spending. If the permitted route is unavailable, keep the handoff ready
and report that condition instead of changing implementation ownership.

### Implementation handoff readiness

Prepare upcoming tasks against the actual predecessor APIs. Each bead names the
production entry point and call path, existing functions/types to reuse or extend,
required inputs, and the files its implementer may change. Before dispatch,
confirm those interfaces supply the required inputs and ownership permits their
composition. If a predecessor is unfinished, record the assumption and recheck it
after that predecessor lands; future API names are not existing implementation.

Resolve hard authority, concurrency, and recovery mechanisms in the accepted
design before dispatch. Specify the algorithm and failure behavior where a
high-level invariant alone leaves a material correctness decision unresolved.
Split a task when it introduces several independent correctness mechanisms;
its final integration task connects reviewed mechanisms and proves the real
production flow. Ordinary coding steps do not need separate beads.

Keep one complete current acceptance list in the native acceptance field, with
critical requirements mapped to observable checks on the production path.
Description and machine acceptance commands must agree with that list. Keep
superseded decisions in notes and preserve frozen historical inputs. New test
names are implementation deliverables until the required cases actually run.
Before review, the implementer maps each critical requirement to its code
location and executed check, and identifies unresolved items in the existing
concise handoff. Do not create a separate coverage receipt.

Apply preparation in proportion to the task's risk as it approaches dispatch;
the short routine-fix handoff is sufficient when no material interface or authority
decision is unresolved. Preserve active repair scope and existing ownership. Use
existing session/run timestamps when reporting elapsed time; session counts alone
do not measure time spent. No separate handoff metrics report is required.

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

### Coordination and session reuse

Keep one implementer session through implementation, repair, and delivery, and
reuse the assigned independent review session where available. A completed model
turn is a handoff point, not a reason to start another session. If a session is
unavailable, reconcile pending effects and record the replacement owner/session
in the same bead before continuing. Closed beads do not regain execution
authority through session resumption; further work needs its applicable scope
and release authority.

Update the bead when work starts, blocks, reaches review, needs changes, or is
delivered. Keep routine tool calls in existing session/run logs. The current
handoff records session/model/worktree, findings, checks/evidence, and next action;
avoid repeated full-context copies and separate coordination receipts. Authorized
Luna Codex subagents may prepare bounded planning revisions under Sol review;
implementation routing and execution approvals follow AGENTS.md.

Each repair round addresses the consolidated findings in the same implementer
session and reruns failed or affected checks. After two unsuccessful repair
rounds, Sol classifies the unresolved cause (missing design, missed explicit
requirement, implementation defect, or changed scope) and chooses a bounded
correction before another attempt. Carry the investigation timebox across
sessions and retries. Acceptance, protected checks, and spending authority still
apply; this diagnosis is not a new automatic release gate.

### Gangway dispatch and resume

`agents/opencode-worker.md` is the portable direct-worker prompt. Where the global
OpenCode configuration loads `~/.config/opencode/agent/slugineer.md` as both default
agent and global instructions, install this prompt at that path to remove the
legacy coordinator and foreign-project rules. Keep provider/permission settings
intact. Loaded sessions retain old instructions: reconcile outstanding operations
before an operator reloads OpenCode, then select the advertised agent explicitly.
Do not restart the factory as part of an instruction update.

Use Gangway's advertised tool schemas; source installation alone does not prove
the loaded endpoint exposes new fields. If `requestID` is absent, report the
capability mismatch and arrange a safe plugin/client reload. Do not restart a
service with active work or silently treat legacy dispatch as retry-safe.

1. Dispatch with an explicit advertised agent, model, directory, and `requestID`.
   Inspect advertised capabilities and agent availability first; do not assume a
   worker name exists or accept a coordinator default. Use Flash for implementation
   and independent routine review. Check loaded global instructions for conflicting
   delegation, verification, or delivery rules before dispatch; selecting another
   agent does not remove global instructions. Make IDs unique
   across repositories, beads, roles, and operations, for example
   `agalma:<repo-key>:<bead>:builder:initial` and `...:builder:repair-1`.
   Retain the returned session/request handles in the current bead handoff.
2. Reuse an ID only for the identical effective request. Changed input needs a
   new ID; a repair uses `opencode_prompt` with the existing implementer
   `sessionID` and a new request ID. Keep reviewer operations in their separate
   session. Never use a new ID to evade an unknown delivery outcome.
3. Allow only one outstanding operation per session. Gangway currently binds
   results using a transcript boundary rather than a proven upstream turn ID;
   overlapping turns can invalidate that association. Do not assume cross-process
   deduplication or storage durability beyond Gangway's documented guarantees.
4. Prefer `opencode_wait` with both handles and an explicit `timeoutMs` at or
   below 45000 (and below the actual client limit). A wait expiry preserves the
   running operation; it is not an instruction to interrupt or redispatch.
   Continue useful independent work between bounded waits. Use compact
   `opencode_status` with `requestID` for diagnosis or state inspection; do not
   routinely chain wait, status, and result calls for the same observation.
5. Inspect output returned by completion; use bounded `opencode_result` with
   `requestID` when output is needed. Respect `unchanged`; use `force` only for a
   deliberate reread. Session idle/succeeded and operation completion do not
   establish task acceptance, successful checks, or review approval.
6. Treat unknown delivery as unresolved. With a known session handle, make one
   bounded observational reconciliation (`opencode_status` with `requestID` and
   `reconcile: true`) if needed. Without a recoverable handle, or if the outcome
   stays unknown, record the blocker and stop dispatch. Reconciliation never
   resends prompts or creates sessions. Resume only after delivery is established
   and the existing authority permits the next operation.

Beads owns task definitions and progress; Gangway tracks session operations.
Agalma retains authorization, scheduling, and review decisions. This protocol
adds no automatic bead mutation, task release, or paid-provider authorization.

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
implemented by Flash. Ephemeral data is not a reason to build another tool.

Choose tests by changed behavior and risk, following docs/testing-strategy.md.
Focused existing regressions or table-driven cases normally suffice for local
config/path changes. Require properties for named changed invariants that warrant
generated inputs or sequences, especially recovery, concurrency, accounting, or
authorization; do not impose a new generator or fixed quota on every fix. Reuse
existing properties where sufficient. Required properties use independent
assertions, bounded case budgets, and replayable seeds or deterministic generation.
Persist failing inputs and concrete regressions. New dependencies/frameworks require
explicit design agreement. Frozen acceptance and protected checks remain mandatory.
Documentation-only changes need content review and whitespace checks.

Retain evidence for every run under `target/test-runs/<bead-id>/<run-id>/` at a
recorded persistent root: exact commands/environment, source identity, raw logs,
exit status, results, and applicable replay seeds, case budgets, and fixture data.
Passing and interrupted
runs are retained too. Use ScratchDir's retention APIs where necessary; never
overwrite prior runs or delete evidence without explicit user direction. Delivery
includes all new test sources, generators, and fixtures; worktree removal requires
confirming those sources and run evidence survive outside the removed worktree.

Run focused checks first. Broaden to full acceptance once when the task requires
it or a concrete integration risk justifies it. After checks pass, repeat only
for relevant changes or unresolved concerns. Documentation-only amendments need
link/content review and whitespace checks, not a Cargo suite.

For routine work, existing runner/session logs, exit status, source identity, and
one concise handoff are sufficient. Do not create custom receipt tools or duplicate
reports; add metadata or retained fixture data only where needed for replay or a
named acceptance contract. Preserve existing logs and stronger immutable evidence.

Keep one handoff with the bead, files changed, commands, results, raw log paths,
remaining uncertainty, and next action. Separate reported evidence from checks
the reviewer ran. Partial results from different source states are not one
coherent full run; a subset of matching source hashes does not prove otherwise.
Preserve historical receipts and the stronger evidence required by protected
evaluation and promotion contracts.
