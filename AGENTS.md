# Agalma architecture and working rules

## Roles and implementation ownership

These working rules govern repository collaboration. They do not change the
factory's runtime routing table, task-release authority, or promotion policy.

- Sol owns architecture and planning, and handles review for high-difficulty
  tasks or when explicitly requested. Routine implementation review is assigned
  to DeepSeek Flash in a separate review session. Sol sessions must load the
  [architect persona](agents/sol-architect.md) and follow the
  [development workflow](docs/development-workflow.md).
- Sol may edit documentation, designs, agent instructions, and beads directly.
  Sol may inspect code and run existing, scoped checks to resolve a concrete
  review question.
  When authorized by the user, Luna Codex subagents may draft or revise bounded
  planning fields under Sol review. That delegation grants no implementation,
  release, spending, or promotion authority.
- All code implementation goes to `opencode-go/deepseek-v4.1-flash` through OpenCode.
  Do not dispatch Luna through OpenCode.
  This includes source, implementation tests, scripts, build configuration,
  generated code, debugging helpers, and small fixes. Sol specifies the change;
  DeepSeek Flash reviews routine implementation work, while Sol reviews
  high-difficulty work. Review findings return to the implementer for repair.
- An unavailable implementation model does not transfer code ownership to Sol.
  Keep the bounded handoff ready and report the unavailable route.
- Discuss architecture, scope, and tradeoffs with the user before implementation.
  Carry accepted decisions forward without repeatedly asking for confirmation.
  Do not expand the agreed scope or introduce a new framework during repair.
- Use this cycle: discuss -> Sol prepares a bounded bead -> Flash implements
  and tests -> DeepSeek Flash reviews routine work (Sol reviews high-difficulty
  work) -> the implementer repairs findings -> close when the agreed acceptance
  is satisfied.
- Reuse one implementer session and task worktree through implementation,
  repairs, and delivery. Keep the assigned reviewer independent and reuse its
  review session where available. Replace an unavailable session only after
  reconciling pending work and recording the ownership handoff.
- Return one consolidated blocking-findings list per review round against the
  accepted scope. After two unsuccessful repair rounds, Sol diagnoses missing
  design, missed requirements, implementation defects, or scope changes before
  another attempt. Diagnosis does not waive acceptance or authorize extra spend.
- For routine fixes, use one short handoff: outcome, owned files, acceptance
  checks, scope/effort limits, and stopping condition. Dispatch directly to an
  explicitly selected project agent `agalma` using Flash, after confirming it is
  advertised; no coordinator, child agents, or decomposition for a single bounded
  fix. Keep
  routine review in a separate Flash session. Use existing tools and fixtures;
  new harnesses, policy generators, and independent prerequisites require separate
  scope agreement. Mechanical fixture updates alone do not justify new tooling.
- Reassess routine work at about 15 minutes of elapsed effort or after the first
  repair round, whichever comes first. Identify the cause and a bounded next step
  before continuing; stop expansion for scope agreement when needed. This is a
  scope checkpoint, not permission to skip acceptance, grant spend, or retry
  indefinitely. Carry the checkpoint across session resumes.
- Reviews block on concrete acceptance failures, regressions, or contract
  violations. Preferences and unrelated improvements are nonblocking follow-up
  suggestions. After approval, the same Flash implementer owns commit, authorized
  merge, clean worktree/cache removal, and bead closure; do not add a new planning
  or verification phase. Report any unavailable delivery authority or route.
- Update beads at meaningful transitions: started, blocked, ready for review,
  changes requested, and delivered. Ordinary tool calls stay in session/run
  logs. Keep one current handoff; reuse automatically retained test evidence.
- Implementation and repair take place in a named Git worktree on a task branch,
  with the bead ID in the worktree name: `target/worktrees/<bead>` on
  `task/<bead>`, with its redirected Cargo cache at
  `target/worktree-caches/<bead>/target`. Record its path, branch, and base commit
  before editing. Reuse that worktree for retries and review repairs; do not use
  one-off temporary directories, disposable clones, or the shared primary checkout
  for implementation. Coordinate owned files and shared build outputs. Retain the
  worktree through review and repair. After the reviewed task changes are committed
  and merged into `main`, remove its clean task worktree and reclaim its redirected
  build cache as part of delivery; no further confirmation is needed. Prefer
  `make worktree-remove BEAD=<id>`, which refuses a dirty or unmerged worktree and
  refuses while Cargo is running. First confirm all test sources are delivered and
  preserve all run evidence outside the worktree. Preserve dirty or unmerged
  worktrees and report the remaining work; never force removal or discard changes.
- After the assigned reviewer approves acceptance, commit the completed task's
  reviewed changes before marking delivery complete. Stage only that task's files
  or hunks;
  preserve unrelated and unreviewed work. A shared dirty workspace is not a
  reason to commit every pending file. Commit with an explicit pathspec
  (`git commit -- <paths>`) whenever the index may already hold another session's
  staged files, and restore any such entry during retries rather than dropping it.
  Report overlapping ownership that prevents a clean task commit. Sol may commit
  reviewed documentation and instructions.
- Stamp bd authorship: a model session that creates or updates a bead passes
  `--actor <model-id>` (or exports `BEADS_ACTOR`), using its canonical model ID
  such as `gpt-6-sol` or `opencode-go/deepseek-v4.1-flash`. The
  authoring model is the recorded author; never let the default git/shell
  identity stand in for it.
- Stamp bd closure with the implementation model: the close reason includes
  `implemented-by: <model-id>`, e.g.
  `bd close agalma-xyz --reason "implemented-by: opencode-go/deepseek-v4.1-flash; <acceptance outcome>"`.
  The close's `--actor` stays the session that runs the close, which differs
  from the implementer when Sol closes after review.

## Reusable builds

- Use the root [Makefile](Makefile) for routine workspace builds. `make`,
  `make all`, and `make build` compile all product workspace targets, including
  libraries, server binaries, examples, tests, and benches. They do not run tests.
- The same Makefile is the single entry point for scoped checks and build-artifact
  hygiene: `make test-focused SELECTION=...`, `make test-peers`, `make test-full`,
  `make report`, `make sweep` and `make sweep-apply`,
  `make worktree-remove BEAD=<id>`, `make clean-build`, and `make clean-caches`.
  Add new build, test, or hygiene entry points here rather than as per-task
  wrapper scripts.
- Use `make release` for the same targets in the release profile and `make help`
  for usage. Cargo discovers workspace members; do not add per-component build
  lists when a new module joins the workspace. Standalone spikes and fixture
  projects are outside this build scope.
- Builds default to `--offline --locked` and reuse Cargo's normal `target/`
  cache. Override `CARGO` for the Cargo executable, `CARGO_FLAGS` for Cargo
  options, and `CARGO_TARGET_DIR` for an established alternate cache. A flag
  override does not authorize dependency downloads or lockfile changes.
- Coordinate builds with sessions using shared server binaries. Reuse caches;
  isolate outputs only when concurrent work would overwrite required binaries.
  Build success establishes compilation; choose test scope separately below.
- A build script must not register its own `OUT_DIR` output as a `rerun-if-changed`
  input: a self-watched output makes Cargo rerun the script and rebuild every
  dependent crate on every invocation, so warm builds never become no-ops.
  `scripts/check-noop-rebuild.sh` guards the `agalma-ipc` no-op property (a second
  identical build must not recompile); run it when changing a build script.
- Build inside an implementation worktree only with a redirected cache:
  `CARGO_TARGET_DIR=target/worktree-caches/<bead>/target`. Do not compile into
  `<worktree>/target/`; that duplicates a multi-gigabyte cache per worktree.
  Evidence runs default to `CARGO_INCREMENTAL=0` through the Makefile; set
  `CARGO_INCREMENTAL=1` only for fast local iteration.
- A redirected cache and `<worktree>/target/` are build output, not evidence: a
  cold one is 0.5-4 GiB because it holds the full dependency graph, native test
  executables, and (unless disabled) incremental IR. `make report` shows
  consumption, `make sweep` classifies caches, and `make sweep-apply` removes
  caches whose worktree is gone or whose task branch is merged into `main`.
  Never remove a registered, dirty, or unmerged worktree or its cache, and never
  sweep `target/test-runs` evidence.

## Verification and session discipline

- State the intended outcome, role, and test scope before starting work. Explain
  what behavior a check establishes and what remains untested.
- Use one repository test entry point with explicit `focused`, `peers`, or `full`
  scope: `make test-focused SELECTION=...`, `make test-peers`, `make test-full`,
  or the runner directly as `bash scripts/test.sh <scope> [selection]`. The unified
  runner (`scripts/test.sh`, agalma-i15) is implemented and is the single entry
  point; scope and selection rules are in `docs/development-workflow.md` and
  `docs/testing-strategy.md`. It validates an explicit selection, compiles the
  selected targets once, runs them through a bounded concurrent pool honoring
  per-unit isolation groups, and writes raw logs and exit status under
  `target/test-runs/<bead>/<run-id>/`. It requires `jq` (selection validation and
  executable discovery) and `rg` (peer scope). Do not create another temporary wrapper.
- Select checks from changed behavior and affected consumers. Name explicit
  test targets or unit-test modules; a whole crate is not the default focused
  selection. Follow docs/testing-strategy.md for selection and safe concurrency.
  Each implementation handoff names the behavior, selected targets/filters,
  required binaries, resource conflicts, and stopping condition. Shared contract
  changes include relevant consumer and recovery tests even in unchanged files.
- Do not routinely run whole-crate or workspace suites before or after editing.
  Use a baseline only to reproduce a concrete failure or satisfy required paired
  evidence, with the necessary selection. A whole-crate run needs a recorded
  coverage reason; a full suite runs once when acceptance or a named integration
  risk requires it. Preserve frozen acceptance and protected promotion checks;
  narrowing them requires the existing amendment authority.
- Run independent selected checks concurrently where isolation is established,
  with a bounded CPU and process budget. Coordinate prerequisite compilation in
  the shared Cargo cache before overlapping test execution. Competing Cargo
  builds against one cache are not the concurrency strategy. Serialize unknown
  or conflicting ports, fixture paths, services, and executable ownership.
  Case-level serialization does not prevent conflicts between test binaries.
  Use the implemented runner: per-unit `--isolated`/`--group` markers and its
  conservative default (an unmarked unit serializes with every other unit) encode
  the isolation decision explicitly.
- After repair, rerun failed checks and checks affected by the repair. Repeat
  passing checks only when their inputs or prerequisites changed or a named
  unresolved finding requires it. Paid live-provider checks are separate and
  require the applicable explicit authorization.
- Required cases must execute; a missing executable, a skipped required case,
  or a zero-case selection is not a pass. Keep partial, interrupted, and complete
  results distinct. Claim only the behavior covered by the recorded evidence.
- Scale verification to changed behavior and risk. Focused existing regressions
  or table-driven precedence, fallback, and error cases normally suffice for a
  local config/path change; do not require a new generator or fixed case quota.
  Require property tests when a named changed invariant warrants generated inputs
  or operation sequences, especially recovery, concurrency, accounting, or
  authorization. Existing tests can satisfy that requirement. Define properties
  independently of the implementation; retain required generators, replay seeds
  or deterministic generation, case budgets, and failing inputs. Reuse existing
  infrastructure; new frameworks or dependencies need explicit design agreement.
  Frozen acceptance and protected promotion requirements remain mandatory.
  Documentation-only changes need content review and whitespace checks.
- Bound failure investigation to one reproducible example and a stated stopping
  condition. Turn a larger environment or build problem into a separate bounded
  implementation task instead of repeatedly sampling, rebuilding, or retrying.
- Reuse build caches and established artifact directories. Isolate build outputs
  when concurrent work would overwrite binaries; reuse that isolated cache.
  Put run artifacts under `target/test-runs/<bead-id>/<run-id>/`, not new
  system-temp structures. Record a persistent artifact root outside any worktree
  scheduled for removal, and include its absolute path in the handoff.
- Save every new test, generator, and regression fixture in the task's reviewed
  repository changes. Retain raw logs, exact commands/environment, source revision
  and pending change identity, results, and applicable replay seeds and fixture
  data for every test run,
  including passing, failed, and interrupted runs. Keep run outputs separate so
  retries cannot overwrite earlier evidence. Use ScratchDir retention APIs where
  needed; automatic fixture cleanup must not discard required run evidence.
  Before worktree removal, confirm all test sources are delivered and all run
  artifacts remain at the recorded persistent root. Do not delete test evidence
  without explicit user direction.
- Prefer existing tools and native commands. Recurring executable logic belongs
  in a reusable repository tool implemented by Flash. Do not turn a one-off
  configuration or inspection step into a reusable tool merely to satisfy this
  rule. Sol does not write disposable Python or shell helpers for logging,
  counting, hashing, or handoffs.
- Keep raw logs, exact commands, exit status, and one concise handoff. Do not
  create overlapping receipts for routine verification. Preserve stronger
  immutable evidence where a versioned acceptance or promotion contract requires
  it; never rewrite a frozen historical receipt to match current files.
- Report the useful outcome, remaining uncertainty, and next decision. Agree on
  an effort limit for costly investigations and stop at that boundary with a
  usable handoff. Routine narration must not obscure the outcome.

## Context and tool-output discipline

- Start backlog triage with compact fields: ID, title, status, priority,
  defer/release state, and direct blocking IDs. Inspect related tasks' status
  separately; do not print their embedded descriptions or parent designs.
- For a selected task, read its own scope, design, acceptance, notes, and file
  ownership once, selecting those fields from bd JSON before returning output.
  Full unfiltered `bd show` output is not the default. Fetch additional source
  sections only to resolve a named uncertainty; truncation is not a complete read.
- Reuse instructions, contracts, and task text already loaded in the session.
  Refresh compact state when it may have changed; reread text only after a
  relevant edit or when missing context affects the decision. Do not repeatedly
  load whole epics, roadmaps, receipts, or workflow documents at every stage.
- Give implementers the bead ID, canonical document paths, ownership, acceptance
  scope, and stopping condition. They read their complete assigned task locally.
  Avoid duplicating full bodies in planner output or dispatch prompts. This does
  not weaken frozen-input, immutable evidence, or complete-context requirements.
- Discover tool names first, then inspect only relevant tool schemas. Filter
  process listings, logs, and searches before returning them. Exclude .beads,
  build outputs, and embedded records from broad source searches unless needed.
  Save required raw evidence in artifacts and return a bounded summary.
- After a dispatch failure, reconcile whether a session was created before
  retrying. Bound bridge diagnosis to one reproducible failure and a stated
  effort limit. If the route remains unavailable, retain the handoff and report
  the blocker; do not turn dispatch into unbounded infrastructure investigation.
- Follow the development workflow's Gangway protocol: check advertised tool
  capabilities, assign a stable request ID per exact submitted operation, use
  bounded waits, and allow only one outstanding operation per session. A repair
  uses a new request ID in the same session. Unknown delivery never authorizes
  resubmission or a replacement session; observe/reconcile before any retry.
- Report dispatched work with bead ID, session ID, model, directory, and observed
  status. Queue readiness, authorization, claimed state, successful dispatch,
  and completed acceptance are distinct; never present one as proof of another.

## Architectural goal

Agalma is a hub-and-spoke software factory. The hub is the conductor and its
control plane. Spokes are independently installable plugins. Adding a module
should mean creating a plugin, registering it, and letting it report data to the
hub UI without editing or rebuilding the hub.

These rules define the target architecture. Existing compiled wiring, fixed UI
component lists, direct SQLite helpers, and peer connections are migration work;
do not extend those patterns when adding plugins. Do not claim a boundary has
migrated until the production path uses it and the acceptance evidence proves it.

## Plugin packaging and lifecycle

- A plugin runs as an independently launched, supervised process. Its package
  supplies an executable, manifest, configuration schema, and protocol version.
- Discover plugins from configured plugin directories. Do not require a new
  conductor import, environment selector, or hardcoded UI entry for each plugin.
- The hub registry and supervisor own registration, startup, health, drain,
  upgrade, and removal. Resolve declared dependencies through the registry.
- Preserve existing binding IDs, versions, generations, capability negotiation,
  and staged activation. Pending operations retain their original binding until
  completion or cancellation and reconciliation.

## Contracts and communication

- Every plugin implements a small common contract describing its identity,
  capabilities, configuration, health, reports, and available actions.
- Keep specialized functions behind typed domain contracts such as `HarnessApi`
  and `WorkspaceApi`. The common plugin contract provides management and
  reporting; it does not replace domain semantics with arbitrary commands.
- Shared envelopes and identifiers belong in canonical contracts. Plugin-specific
  payloads are namespaced and versioned; adding a reporting plugin must not
  require adding its private types to the hub.
- The hub owns authorization, registration, and routing. Spokes request declared
  capabilities through the hub; they do not discover peer sockets, import peer
  implementations, or access another component's private storage.
- Consumers access the ledger through its declared API via hub routing. Only
  the ledger implementation opens its database or issues SQL against it.

## Reporting and UI

- Reports use a common envelope containing plugin identity, binding generation,
  payload schema version, sequence, timestamp, and execution reference when
  relevant. Distinguish transient health updates from durable observations.
- Support refresh from a snapshot when updates are dropped. The hub must display
  stale or disconnected plugins explicitly; socket presence alone is not health.
- The control plane derives component navigation and detail views from the
  registry. Plugins supply data and declarative view descriptions using supported
  building blocks: status, key/value metrics, tables, timelines, alerts, and
  artifact links. Standard reporting requires no custom hub UI code.
- The hub UI renders reports and exposes declared actions. It does not acquire
  plugin business logic or infer new workflow behavior from reported data.

## Actions and workflow integration

- Reporting may tolerate dropped updates and recover from snapshots. Effectful
  actions require operation IDs, input identity, deadlines, durable receipts,
  cancellation, and reconciliation against the pinned plugin version.
- Plugins advertise actions; the hub validates their capabilities and authority,
  applies stop policy, dispatches them, and records outcomes. UI access does not
  grant additional authority.
- Automatic UI appearance does not imply automatic factory workflow integration.
  Creating tasks, triggering builds, or changing scheduling requires a declared
  capability and explicit scheduling policy.

## Delivery and acceptance

Build the shared platform in dependency order: manifest/protocol, registry and
supervisor, reporting ingestion, dynamic UI, actions and recovery, then migration
of an existing spoke. Router process extraction should build on this platform
rather than add another bespoke supervisor and UI path.

The platform acceptance test is: install a previously unknown plugin, launch it,
see its data and health in the UI, invoke a declared action, restart the hub, and
retain its identity and receipts, all without editing or rebuilding the hub.
Contract-only or isolated component tests do not establish this end-to-end claim.

## Self-mutation policy

- Follow the versioned contracts in `docs/self-mutation-contracts.md` and the
  staged roadmap in `docs/self-mutation-roadmap.md`.
- Start automatic mutation with allowlisted builder prompts and builder-context
  skills. Plugin packages follow through an explicit allowlist; core code retains
  transitional oversight until P1 evidence enables its versioned policy.
- Optimize task success first. Protected regressions veto promotion; missing
  usage or evaluation evidence cannot establish a win.
- Freeze complete task inputs and role/binding/oracle references before dispatch.
  Recovery never reconstructs historical acceptance from current bd contents.
- Automatic task release requires a durable directive-authorized receipt and
  revalidation at claim. A cleared defer date or model-authored label is not authority.
- Human execution authorization for a staged planner/queue child is a native
  human execution-approval gate: labels `human,execution-approval`, metadata
  `approval_for=<child>`, and a direct `blocks` edge, resolved only by an
  explicit human `bd gate resolve`. Decomposition approval, the `staged` label,
  and a cleared defer date grant no execution authority; `defer` is deliberate
  postponement. Missing, conflicting, or malformed gate associations fail
  closed.
- Use SHA-256 for immutable package/evidence integrity, inspected request paths
  for bounded inference, and separate cash/quota/local resource accounting.
- A binary rollback never rewinds the live ledger schema. Core activation and
  rollback belong to an external recovery owner with retained compatible runners.
