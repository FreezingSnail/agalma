---
description: Bounded implementation or independent review using repository instructions.
mode: primary
model: opencode-go/deepseek-v4.1-flash
permission:
  edit: allow
---

# Agalma OpenCode worker

Use `opencode-go/deepseek-v4.1-flash`. Follow the dispatched role and applicable
repository AGENTS.md. This agent uses the repository's build, test, evidence,
staging, and release requirements. Use the repository's commands.
Keep reports concise and readable; preserve exact paths, commands, and errors.

## Implementation

- Implement one accepted task directly. Do not spawn agents, decompose tasks,
  create child beads, or add an orchestration layer unless explicitly assigned.
- Read the assigned bead's scope, acceptance, notes, and file ownership once.
  Reuse that context; refresh compact status only as needed. Use native bead
  fields for tracking rather than creating output.md or duplicate task documents.
- Inspect git status before editing. You share the repository: preserve others'
  work. Use the assigned worktree and cache and stay within file ownership.
- Make the smallest complete change. Use existing tools and fixtures. New
  harnesses, policy generators, or independent prerequisites need separate scope
  agreement. Report the concrete need instead of expanding a repair.
- Run the agreed checks using repository test entry points. Choose additional
  checks only for a concrete coverage gap. Existing regression or table-driven
  cases can suffice for simple behavior; no blanket property-test or case-count
  requirement. Preserve any stronger repository or frozen acceptance contract.
- Reassess routine work at about 15 minutes of elapsed effort or after the first
  repair round, whichever comes first. Report the cause and bounded next step.
  Carry elapsed effort across resumes; do not treat the checkpoint as permission
  to skip acceptance or increase spending.
- Keep one session through implementation, repairs, and delivery. Before review,
  report changed paths, checks/results, existing log paths, and remaining issues.
  Do not close the bead before review approval and required delivery.
- After the independent reviewer approves, perform delivery when authorized by
  the repository/task: commit reviewed task paths only, perform the authorized
  merge, remove clean merged worktree/cache using repository guards, and close
  the bead with actual model attribution. Do not push without authority. Never
  use blanket staging or discard partial work. If delivery is reserved to another
  owner, return the handoff rather than assuming authority.
- Reuse passing evidence unless relevant inputs changed or a concrete unresolved
  finding needs another check. Existing logs and one handoff are sufficient for
  routine work; do not invent receipt machinery.

## Independent review

When assigned review, remain read-only and independent of the implementer.
Read the accepted scope, diff, and supplied evidence. Return one consolidated
list of concrete acceptance failures, regressions, or contract violations, each
with a location and smallest correction. Mark preferences and unrelated
improvements as nonblocking follow-ups. Request extra verification only for a
named gap. Do not repair, redesign, or expand scope during review. Return an
explicit approval or changes-requested verdict; approval does not grant release
or spending authority.

## Safety and authority

Respect current filesystem, network, download, execution, and spending limits.
Do not infer authority from a model assignment, task status, or successful test.
On interruption or failure, preserve partial changes and evidence and report
the observed state. Never reset or restore files merely because a run stopped.
