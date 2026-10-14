# Using Beads in this repository

Use `bd` as the work tracker. The repository rules in [AGENTS.md](../AGENTS.md)
and the detailed [development workflow](../docs/development-workflow.md) are
authoritative; this page is a quick reference for Codex sessions.

## Find and inspect work

- Start triage with compact fields: ID, title, status, priority, defer/release
  state, and direct blocking IDs. Avoid printing every bead description.
- Check direct blockers' statuses separately. `ready`, a satisfied dependency,
  or an elapsed defer date does not by itself authorize release.
- For a selected bead, read its complete scope, design, acceptance, notes, and
  owned files once. Keep parent and dependency descriptions out of the output
  unless a specific uncertainty requires them.
- Before dispatch, verify the bead and blockers are current, preserve the exact
  task input and base revision, and name the implementation owner and acceptance
  scope.

Example compact view:

```sh
bd list --json | jq '[.[] | {id,title,status,priority,defer_until}]'
bd show <id> --json | jq '.[0] | {id,title,status,priority,defer_until,description,design,acceptance_criteria,notes,dependencies:[.dependencies[]|{id,title,status,dependency_type}]}'
```

Use `bd prime` for current, version-matched CLI guidance and `bd <command>
--help` for command details. Treat generic guidance as supplemental: this repo's
`AGENTS.md` controls actor stamping, release authority, ownership, and review.
In particular, a task appearing ready in `bd prime` does not authorize release.

The upstream [Beads Codex plugin](https://github.com/gastownhall/beads/tree/main/plugins/beads/.codex-plugin)
packages `plugin.json` and lifecycle hooks separately from its `beads` skill.
This repository guide is intentionally repo-specific and does not install those
hooks or copy that skill.

## Update, release, and close

- Stamp every bead create/update with the acting model's canonical ID using
  `--actor <model-id>`. Never use the shell or Git identity as a substitute.
- Staged/deferred work remains unreleased until an operator explicitly releases
  it. Record that authorization. Revalidate direct blockers before dispatch.
- Keep queue readiness, release, claim, dispatch, and acceptance as separate
  states. Mark work in progress only after a dispatch succeeds.
- Keep implementation inside the bead's file ownership and acceptance. Record
  exact commands, exits, and raw focused-check logs under `target/test-runs/`.
- Close only after review accepts the evidence. Include
  `implemented-by: <canonical-model-id>` and the acceptance outcome in the close
  reason; the actor is the model that performs the close.

When a command or policy is unclear, consult `bd --help`, the bead's complete
handoff, and the canonical files above before changing queue state.
