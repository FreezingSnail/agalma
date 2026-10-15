# Intake + planning pipeline — concierge → designer → staged decomposition

Bead: `agalma-i5h.1` (CP-D1). Epic: `agalma-i5h`. Orchestrator-authored; gates all
CP beads. Ported from maduin (`harness/agents/slugineer-planner-concierge.md`,
`slugineer-planner-designer.md`, `harness/maduin-decomp.el:241-324`,
`.agents/planning/maduin/design/v0.2-orchestration.md:104-112,187-221`).

Goal: a stated goal ("stand up TUI dashboard") travels intake → epic + design →
decomposition → deterministic validation → human approval → fleet execution,
with LLM sessions supplying content and Rust code deciding consequences.

## Roles

- **Concierge** (intake; interactive or one-shot): rough idea → epic(s) + design
  doc. Clarify ≤3 questions. Scope to 8–12 decomposable children per epic; split
  larger ideas into multiple epics with explicit inter-epic dependency direction.
  MUST NOT decompose, create children, fill acceptance, defer, or implement.
- **Designer** (autonomous, one-shot): reads the epic design (source of truth),
  spikes unknowns, authors child tickets + dep edges, runs the self-audit.
  MUST NOT implement, review, repair, resolve an approval gate, undefer, or
  claim children.
- **Validator** (deterministic Rust): judges the decomposition; pass/fail with a
  rule-named violation list.
- **Human**: approves by resolving a child's native human execution-approval
  gate (`bd gate resolve <gate>`); rejection feedback is a comment,
  design revised in place (never supersede).

## bd conventions

- Epic: `bd create "<one-line ASCII title>" -t epic --body-file <desc>` then
  `bd update <epic> --design-file <design>`.
- Child: `bd create "<title>" -t task --parent <epic> --body-file <desc>
  --design-file <design> --labels staged,difficulty:low|high`; created **open**
  (no `--defer` sentinel), and each child gets exactly one native human
  execution-approval gate (CP.3 / `agalma-i5h.8`) that pins it.
- Child description fixed headings, in order:
  `# Scope`, `# Files`, `# Contract`, `# Invariants`, `# Non-goals`, `# MACHINE`.
  MACHINE keys exactly one per line: `provides:`, `consumes:`, `files:`, `tier:`.
- Caps: description prose ≤400 words (excluding `# Contract`/`# MACHINE`);
  acceptance ≤80 words, one plain-ASCII line; design ≤150 words for low tier.
- Every executable child carries the factory intake block (`directive/v0` YAML
  fence: target, ref, acceptance list, budget_usd, priority, family,
  max_attempts) authored by the designer; acceptance commands are shell-safe and
  scope-guarded. The validator parses it with the existing taskqueue parser.
- Labels: `staged` + exactly one `difficulty:*`; after the gate,
  `decomp-approved` or `decomp-rejected`; rework task label `decomp-fix`;
  review-gate label `drift-fix`; human escalation label `human`; native human
  execution-approval gate label `execution-approval`.
- Native human execution-approval gate (the human gate primitive). The designer
  authors one gate per child in the same session that creates the child:

  ```
  bd gate create --type human --blocks <child> --reason "Execution approval for <child>"
  bd update <gate> --add-label human,execution-approval --set-metadata approval_for=<child>
  ```

  The gate is a standalone issue (no parent), pinned to the child by a direct
  `blocks` edge. It is created **unresolved**; only a human resolves it with
  `bd gate resolve <gate>`, which is the explicit bootstrap authorization. The
  designer never resolves a gate and never sets `decomp-approved`.
- Approval state (two independent bits, never auto-run):
  - **validator approval** — `decomp-approved` (never alongside
    `decomp-rejected`), applied by the sweep gate on a clean decomposition and
    exactly one well-formed gate per staged child;
  - **human execution authorization** — the child's associated native human gate
    resolved (closed). Clearing a defer date or changing a status never
    authorizes; `defer` is reserved for deliberate postponement.
- Ready contract: the adapter admits a `staged` child only when it is
  `decomp-approved`, carries no defer date, has **exactly one** well-formed
  associated native human gate (`issue_type=gate`, `await_type=human`, labels
  `human,execution-approval`, `approval_for=<child>`) that a human has resolved,
  and has no unresolved prerequisites. Missing, conflicting, or malformed
  approval associations deny release. The adapter excludes epics, gate issues,
  `human`, `parked`, `decomp-fix`, `drift-fix`, and any issue with a set defer
  date. An accidental `bd update --status open` never releases staged work;
  ordinary open work needs no gate.

## Difficulty tier (derived, never judged)

`low` iff exactly one implementation module/file, no new cross-module interface
(`provides` empty), fully specified contract, ≲100 changed lines, and none of:
async/process lifecycle, external-format parsing, data migration, performance
constraint. Otherwise `high`. Never a model name or effort level.

## Validator rules (pure functions; port of `maduin-decomp.el`)

| Rule | Check |
|---|---|
| `count-budget` | 8 ≤ children ≤ 12 (below 8 fails without an explicit human verdict comment; above 12 fails) |
| `schema-section` | fixed headings present, exact order |
| `cap-prose` | description ≤400 words (minus Contract/MACHINE) |
| `cap-acceptance` | acceptance ≤80 words, single plain-ASCII line |
| `cap-design` | design ≤150 words when tier=low |
| `tier-mismatch` | `difficulty:*` label equals derived tier |
| `interim-value` | rejects "until then", "placeholder", "for now pass nil", "temporary stub" phrasing |
| `unprovided-symbol` | each `consumes:` symbol is `provides:`d by the child or a transitive dependency |
| `graph-cycle` | no dependency cycles |
| `graph-width` | median dependency-wave width ≥ fleet concurrency (serial MVP: 1; parameterized) |
| `intake-block` | `directive/v0` fence parses; acceptance non-empty; budget present |

Violations are emitted with rule names so a `decomp-fix` task can carry an
ordered list. The validator never edits tickets.

## Routing and prompts

- `RouteClass::Planner` gains real candidates, strongest-first:
  `opencode-go/deepseek-v4-pro`, `opencode-go/gpt-6-luna`,
  `opencode-go/claude-haiku-5-5` — all paid/subscription, gated by
  `AGALMA_ALLOW_PAID` + credential presence. When no planner candidate is
  eligible the planner path fails closed with a clear error (never silently
  downgrades to a free builder model). Decisions recorded as `route.model`
  class=planner.
- Concierge/designer prompts are canonical role prompts (`prompts/concierge.md`,
  `prompts/designer.md`) alongside builder/verifier; model assignment stays
  single-source in the routing table, never in prompt files.
- Sessions are ephemeral and task-scoped: one designer session per epic, one
  concierge session per intake; continuity via epic design + bd history.

## Epic sweep (work loop)

After recovery, before claiming ordinary work:

1. Query open epics with zero children, or epics whose latest decomposition was
   `decomp-rejected` with retries remaining.
2. Spawn one designer session (planner route) with the epic design; concurrency
   cap 1 in the serial MVP.
3. On completion: run the validator and reconcile each staged child's native
   human approval gate.
   - Pass (validator clean **and** exactly one well-formed gate per staged
     child) → label children `decomp-approved` (children stay blocked/staged;
     validator approval alone releases nothing). A missing, conflicting, or
     malformed gate association fails closed as an `approval-gate` violation.
   - Fail → label `decomp-rejected` + create one P1 `decomp-fix` task (parent
     epic) carrying the violation list; retry up to 3; then `bd human <epic>` +
     comment and stop looping.
4. Approved children enter `bd ready` only once a human resolves their native
   execution-approval gate (and prerequisites close); rejected and
   never-validated children never do. The sweep never creates or recreates a
   gate: an absurd association is rework, never invented approval.

## Review gate (post-merge, per epic)

When the last child of an epic closes: record the epic start SHA (first child
land), spawn a reviewer session over `git diff <start>..HEAD` vs the epic
design/acceptance; parse exactly one marker line `REVIEW: APPROVED` or
`REVIEW: DRIFT <feedback>`; no marker = error (never silent pass). Drift → P1
`drift-fix` task under the epic and hold ordinary dispatch until cleared;
retry budget then human escalation. (Detailed in CP.5.)

Durable per-epic state (survives restart), mirroring the epic sweep:
`review-start:<sha>` is the diff baseline recorded at the first child land;
`review-attempts:<n>` counts review attempts (reconciled with the `drift-fix`
tasks); `review-drift` is the active dispatch hold (removed on a later
`APPROVED`); `review-approved` is terminal (never re-reviewed); `human` marks
escalation. The reviewer runs on the strongest (planner) route under a
`review:<epic>` scope and fails closed when it is unavailable — never a silent
pass or a downgrade to a free builder model. A `drift-fix` task is rework
bookkeeping (deferred, never ordinary fleet work); the hold withholds ordinary
dispatch until a re-review approves or a human takes over.

## Pilot

`agalma-3kr` (TUI control plane) is the first goal to travel the full path:
concierge design refinement → designer decomposition (existing hand-authored
children are reference input, not binding) → validator → human resolves each
native execution-approval gate → fleet. Success = the dashboard epic's children
are machine-validated, staged, gated, and consumed by the factory.

## Bead plan

| Bead | Scope |
|---|---|
| `.1` | this design |
| `.2` CP.1 | decomposition schema + validator (pure Rust) + `agalma plan-check` CLI |
| `.3` CP.2 | planner routing + concierge/designer prompts |
| `.4` CP.3 | epic sweep + staged decomposition gate |
| `.5` CP.4 | concierge intake session |
| `.6` CP.5 | per-epic review gate |
| `.7` CP.6 | TUI pilot through the pipeline |

## Risks / open items

- Designer session quality on paid models is unproven in agalma; the validator is
  the safety net, and `decomp-fix` keeps rework bounded.
- Per-task factory intake still requires `directive/v0`; the designer must author
  it per child — validator rule `intake-block` enforces parseability.
- `defer` is date-based bd semantics, reserved for deliberate postponement. The
  ready predicate keys on two independent bits — validator `decomp-approved` and
  a resolved native human execution-approval gate — plus no set defer date and no
  open prerequisite, so it never confuses postponed work, still-blocked staged
  children, or rejected work with an authorized child; the `staged` label alone
  authorizes nothing, and clearing a defer date authorizes nothing.

## Planned prompt/skill release extension

The implemented CP gate above remains the bootstrap default. SM.9
(`agalma-cg9.9`) adds a distinct, directive-authorized automatic release service
for allowlisted builder prompts and builder-context skills, per
`docs/self-mutation-contracts.md`. It journals a release receipt before queue
projection and revalidates source digest, active directive, scope and budget at
claim. Decomposition approval, a cleared defer date, or a resolved bootstrap
human gate alone does not grant this automated authority. Plugin packages follow
a later explicit allowlist; bootstrap
code keeps transitional oversight until P1. This amendment is a future contract,
not a claim that the current queue gate already implements automated release.

The closed TUI pilot proved authored/validated/staged decomposition. SF.10
(`agalma-5fy.10`) owns a fresh goal -> design -> staging -> authorized execution
-> independent review -> epic closure proof; retired duplicate TUI work cannot
substitute for that evidence.

SF.11 (`agalma-5fy.11`) adds prerequisite-aware epic review. The current CP.5
implementation only checks staged child closure; it can prematurely review an
epic whose blocking prerequisite remains open. Revision-1 contracts require
visible waiting with no paid reviewer/drift mutation, all-child closure and
graph/design revalidation before approval. SF.10 exercises this guard.
