# Self-build fixture (`fixtures/self-build/`)

The canonical, frozen task definition for the **SF.9 epic-exit self-build proof**
(`crates/agalma-conductor/tests/sf_self_build.rs`). It is a *data* fixture: the
test files it into a scratch, isolated `bd` store and lets the real `agalma work`
production path deliver it, so nothing here is executable on its own.

## Files

- `task.md` — the complete task body: `Scope`, `Files`, `Contract`, `Invariants`,
  `Non-goals`, and the fenced directive block (`directive`/`target`/`ref`/
  `acceptance`/`budget_usd`/`priority`/`family`/`max_attempts`). The conductor
  freezes this text verbatim into `TaskPackage/v1` and renders it into the
  builder prompt, so a builder receives the full specification rather than a
  title.

## How the test uses it

The test binds the fixture with `include_str!` and `bd create --description`
against a scratch git origin plus an isolated `bd` store, then drives
`agalma work --once` in **deterministic scripted transport**
(`AGALMA_ACTIVITY_MODE=scripted`, in-process ledger/workspace/taskqueue). The
scripted builder writes the bounded feature (`pub fn answer() -> u32 { 42 }`)
into the candidate checkout; the *operator* protected baseline then independently
compiles the candidate and asserts `answer() == 42`.

The fixture is deliberately a small feature, not the M0 one-line broken-function
repair: the repository builder prompt must hand a feature builder a multi-section
specification, and the SF.9 proof checks that the frozen sections arrive intact
and that an independent behavioral judge — not the candidate's own test —
decides whether the feature ships.

## Determinism and evidence honesty

- No network, no `bd` daemon, no paid or live provider inference: the run is
  scripted transport only. Nothing here is a vendor or model measurement.
- The scripted build is a deterministic stand-in for the builder turn; a paid
  Flash live run is a **separately gated** step and is skipped when the
  credential/live gate is closed (see `docs/self-build-evidence.md`).
- Scratch state and raw logs live under
  `target/test-runs/agalma-5fy.9/<run-id>/` (persistent, outside the task
  worktree). See `docs/self-build-evidence.md` for the exact commands, run
  identity, and recorded results.
