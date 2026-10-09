# M0.8 — recovery + kill-switch exit evidence

Bead: `agalma-4ui.10` (M0.8). Base HEAD: `cbbfe25`. This is the conductor's M0
exit-gate evidence: recovery reconciliation, survivor termination before
redispatch, kill latch across restart, and CLI fixes.

All run dirs live under `target/test-runs/` (never a system temp directory).

## Recovery matrix

| # | Case | Command | Observed | Result |
|---|------|---------|----------|--------|
| 1 | Crash (SIGKILL) mid-build; survivor terminated with evidence before redispatch | `cargo test -p agalma-conductor --test recovery --offline crash_mid_build_terminates_survivor_before_redispatch` | Scripted build spawns a recorded worker and sleeps; conductor SIGKILLed; orphan alive; restart writes `runs/<exec>/stop/build-1.json` with `process_group_gone=true`, kills the orphan, then rebuilds to done; `main == candidate`, one candidate commit, tag `m0/exec-fix-answer-1` | PASS |
| 2 | Crash after integrate, before completion record; completes without re-merge | `cargo test -p agalma-conductor --test recovery --offline crash_after_integrate_completes_without_remerge` | Child re-exec arms `AGALMA_EXEC_CRASH_AT=after_effect_before_completion` after the integrate effect; parent `recover()` sees `main == candidate`, records the integrate receipt (event `operation_reconciled`), advances to `Done`; `git rev-list --count main == 2` (no re-merge) | PASS |
| 3 | Duplicate delivery of a completed operation | `cargo test -p agalma-conductor --test recovery --offline duplicate_delivery_returns_recorded_receipt` | First `deliver_operation` → `Executed` and one effect line; second → `Recorded` and effect count unchanged | PASS |
| 4 | Kill latch set → restart → no dispatch; `resume` clears; work continues | `cargo test -p agalma-conductor --test recovery --offline kill_latch_survives_restart_then_resume_completes` | SIGTERM persists latch and exits 0; `status --state-dir` reports `kill_latch=true`; restart reconciles the survivor but blocks (failed exit, build effect count unchanged); `resume --state-dir` clears; next run completes (`phase=Done`) | PASS |
| 5 | Surviving worker with a stale lease terminated before redispatch | `cargo test -p agalma-conductor --test recovery --offline stale_lease_survivor_terminated_before_redispatch` | Planted live process-group leader with `started_at_unix_ms=1`; `recover()` terminates it (stop evidence `process_group_gone=true`) without dispatching; the build dispatches only on the next `step` | PASS |
| 6 | Schema mismatch refuses to run | `cargo test -p agalma-conductor --test recovery --offline schema_mismatch_refuses_to_run` | Ledger opened with `schema_version=99`; `agalma run` exits failure with `incompatible ledger schema` and no `runs/` effects | PASS |

Full matrix in one run:

```
cargo test -p agalma-conductor --test recovery --offline
running 6 tests
test crash_after_integrate_completes_without_remerge ... ok
test crash_mid_build_terminates_survivor_before_redispatch ... ok
test duplicate_delivery_returns_recorded_receipt ... ok
test kill_latch_survives_restart_then_resume_completes ... ok
test schema_mismatch_refuses_to_run ... ok
test stale_lease_survivor_terminated_before_redispatch ... ok
test result: ok. 6 passed; 0 failed
```

## Live e2e regression (once)

```
cargo run --offline -p agalma-conductor -- run \
  --fixture fixtures/target-template --once --state-dir target/test-runs/e2e-m0.8

result: phase=Done state=Completed attempt=1 parked_reason=None
EXIT=0

target/debug/agalma status --state-dir target/test-runs/e2e-m0.8
exec:fix-answer:1 phase=Done state=Completed attempt=1 parked_reason=None
kill_latch=false

git -C target/test-runs/e2e-m0.8/runs/exec-fix-answer-1/checkout log --oneline --all --decorate
279b3b3 (HEAD -> candidate/exec-fix-answer-1, tag: m0/exec-fix-answer-1, main) candidate candidate/exec-fix-answer-1
efe66d9 baseline
```

The model fixed `answer()` to `42`; verify went green; integrate landed once and
tagged `m0/exec-fix-answer-1`. No regression.

## Workspace verification

| Command | Result |
|---------|--------|
| `cargo build --workspace --offline` | ok |
| `cargo test --workspace --offline` | ok (all suites; recovery 6, crash_matrix 11, conductor 2, sandbox 3+14, workspace 7, ledger 7, harness 18) |
| `cargo clippy --workspace --all-targets --offline -- -D warnings` | ok (no warnings) |
| `cargo fmt --check --all` | ok (exit 0) |

## Semantics implemented

- **Boot order** (`Composition::build` then `run_command`): ledger schema gate →
  `Executor::recover` = event replay (no I/O) → projection agreement → reconcile
  pending intents → terminate surviving workers via `SandboxApi` with evidence
  (before any redispatch) → `drive` honors the kill latch / `STOP` sentinel
  before every dispatch.
- **Build/verify effects**: each effect writes a durable completion marker
  (`<run>/<phase>/<attempt>.done`) holding the recorded `ActivityOutcome`.
  `effect_present` returns `Present` when it exists, so `reconcile` completes
  without repeating the effect. A live recorded worker is terminated through
  `SandboxApi` (evidence in `<run>/stop/<phase>-<attempt>.json`); a worker that
  died without completing is `Ambiguous` and parks. Verify uses started/done
  markers (one-shot `cargo test`, no long-lived worker identity).
- **Integrate**: `integrate` detects an already-landed merge
  (`Workspace::integrated_receipt`: `main == candidate` and candidate ahead of
  base) and completes the ledger/tag from Git state without re-merging.
- **Worker identity**: `WorkerRecord { pid, pgid, attempt, phase, operation_id,
  started_at_unix_ms }` is persisted at launch under
  `<run>/<phase>/<attempt>.worker.json`; removed on clean completion or after
  termination.
- **Kill switch**: SIGTERM/SIGINT persist `kill_latch` through a second ledger
  connection and exit 0; restart stays blocked; `agalma resume` clears the latch
  and removes the `STOP` sentinel.
- **CLI**: `status` and `resume` now accept `--state-dir` (matching `run`).

## Deviations / open items

- **Scripted mode is test-only.** `AGALMA_ACTIVITY_MODE=scripted` (plus
  `AGALMA_SCRIPTED_BUILD_SLEEP_MS`) selects deterministic build/verify
  stand-ins wired by `activities_with_mode`; the default is the live OpenCode
  harness. Tests use the explicit `ActivityMode::Scripted` argument instead of
  the env var where they run in-process, to avoid env races between threads.
- **Build ambiguous-park branch is conservative.** A worker record whose process
  is already gone with no completion marker parks (`Ambiguous`) rather than
  blindly re-running the model. The matrix exercises the live-survivor branch;
  the dead-worker branch is covered by the same probe logic.
- **Verify has no persisted worker pid.** `cargo test` is a bounded one-shot run
  whose group is torn down by the launcher; a crash mid-verify is detected by the
  started marker and parks. Only the long-lived harness worker carries a durable
  pid/pgid lease.
- **`--once` is unchanged** (still drives to the terminal phase; it is not a
  single-step switch). Out of scope for M0.8.
- **Exit criteria ordering** is enforced inside `Executor::recover`; the explicit
  schema check in `run_command` runs after `Composition::build` (which already
  gates on schema inside `recover`) and is belt-and-suspenders.
- **Termination evidence** for a stray process is obtained through the sandbox
  contract (`SandboxApi::terminate`) but the scripted survivor is an ordinary
  process-group leader rather than a Seatbelt-confined child; the real worker
  path uses `SeatbeltSandbox`.
