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

The session-resume ownership policy from the retrospective's item 1 is deferred.
Do not introduce a new closed-bead resume policy as part of this amendment.

## Reusable build entry point

Use the root [Makefile](../Makefile) for routine builds:

```sh
make             # same as make all / make build
make release     # all workspace targets in the release profile
make help        # targets, build scope, and overrides
```

The debug build runs `cargo build --offline --locked --workspace --all-targets`.
The release build adds `--release`. These commands compile all product workspace
libraries, binaries, examples, tests, and benches; they do not execute tests,
launch services, or build standalone spikes and fixture projects. New workspace
members are included through Cargo without editing a component list in Makefile.

Normal outputs and incremental caches remain in `target/debug/` and
`target/release/`. Override `CARGO` to select the Cargo executable,
`CARGO_FLAGS` to change the default `--offline --locked` options, or
`CARGO_TARGET_DIR` to reuse an established alternate cache. Download and
dependency-change authority still follows the repository rules. Coordinate
shared-cache builds with sessions running process tests against server binaries.

Compilation is separate from verification. Select a test scope below; the
Makefile does not implement the pending unified test runner.

## Unified test entry point

The target entry point is `bash scripts/test.sh <scope> [selection]`.
**It is not implemented yet.** Building it is a Luna/Flash task. This document
defines its contract without claiming that documentation creates a working tool.

| Scope | Selection and purpose | Limits |
|---|---|---|
| `focused` | Explicit crate and optional target/test; verifies behavior relevant to the change | Does not establish workspace acceptance or unselected process boundaries |
| `peers` | The five required ledger, workspace, taskqueue, execution, and sandbox local process smokes | Does not establish paid-provider behavior, all process failure paths, or full conductor acceptance |
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

During the transition, choose one existing documented command per scope:

- Focused: `cargo test --offline -p <crate> [target/filter]`, with required binary
  prerequisites stated separately. This command alone does not build sibling
  server binaries or prove that a selection executed a required case.
- Peer acceptance: `bash scripts/verify-peer-transports.sh`. The existing script
  also builds all workspace binaries and runs the full conductor suite; state
  that broader cost and coverage before using it. It is not a focused peer runner.
- Full: `cargo build --offline --workspace --bins`, then
  `cargo test --offline --workspace`; preserve task-specific environment and
  serialization requirements. An exit of zero does not prove ignored cases ran.

The unified runner should replace routine alternatives. Retain the old peer
script only as a compatibility entry that forwards to the appropriate scope and
preserves any acceptance behavior its callers still require. Do not create
additional ad hoc wrappers during the transition.

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
