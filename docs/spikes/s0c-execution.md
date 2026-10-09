# S0c — execution spike: ledger/dispatch durability

Task: `agalma-1ti` (S0c). Status: complete. Timeboxed spike; kill/fallback decision at exit.
Architecture refs: `docs/architecture.md` §3.6, §13 S0c; `docs/durable-execution-bootstrap.md`
(Acceptance and evaluation); `docs/orchestration-options.md` (evaluation criteria 1–8).

## Purpose

Prove the durability semantics the minimal serial executor depends on, with a
working std-only Rust spike rather than argument:

- atomic commit of a phase transition + event + operation receipt + dispatch
  intent in ONE SQLite transaction;
- stable operation IDs that are unchanged across delivery retries;
- duplicate delivery returns the recorded result and does not repeat the effect;
- durable dispatch intents survive a crash after the transition commit but
  before dispatch;
- restart reconstruction of state from recorded events and reconciliation of
  pending operations;
- reconciliation when an external effect outlived the crash but its completion
  record did not;
- a kill latch that survives restart and blocks dispatch of recovered jobs;
- pinned SQLite/schema/execution versions stored with results, with
  incompatible state parked;
- documented handoff to the B0 bootstrap project.

This is discovery/proof code, not product code. It does not add the spike's
schema or module to the product; it records what the real kernel must preserve.

## Exit criteria

- [x] One SQLite transaction commits state + event + operation receipt + dispatch intent.
- [x] Dispatch intent recovered after a crash between commit and dispatch (case 1).
- [x] Duplicate delivery returns recorded result, effect not repeated (cases 1, 2).
- [x] Conflict rejected when an operation ID is reused with different inputs (case 2).
- [x] Restart reconstructs state from events; projection agrees; pending reconciled (case 3).
- [x] Crash after external effect before completion: effect recognized, not repeated (case 4).
- [x] Kill latch survives restart and blocks recovered dispatch (case 5).
- [x] SQLite/schema/execution versions pinned and stored with results; incompatible state parks (case 6).
- [x] `cargo build --release --offline` succeeds std-only, zero dependencies.
- [x] Exact run commands documented; runner script in-repo.
- [x] Crash matrix filled with pass/fail, command, and evidence per case.

## Pinned versions

| Component | Pinned value |
|---|---|
| Rust toolchain | rustc 1.97.1 (8bab26f4f 2026-07-14), cargo 1.97.1 |
| SQLite CLI | 3.51.0 2025-06-12 (64-bit) at `/usr/bin/sqlite3` |
| Dependencies | none (std only) |
| Journal mode | WAL |
| Durability | `PRAGMA synchronous=FULL` per connection |
| Busy timeout | `PRAGMA busy_timeout=5000` (ms) |
| `schema_version` (physical) | 1 |
| `execution_definition_version` | 1 |

Versions are stored in the `meta` table and, for the execution version, on every
`executions`/`operations` row and inside every recorded result, so recovery can
detect incompatible stored state.

## Layout

```
spikes/s0c/
  Cargo.toml          # zero-dependency, std-only
  Cargo.lock
  run_all.sh          # builds offline, runs all six cases
  src/main.rs         # arg dispatch; parent vs child role
  src/ledger.rs       # SQLite CLI access, atomic commit helper, versions
  src/executor.rs     # transitions, stable op IDs, receipts, intents, reconcile, latch
  src/util.rs         # paths, crash injection, evidence report
  src/cases.rs        # the six crash-matrix cases (fixture pack v0)
  runs/<case>/        # gitignored run artifacts: ledger.sqlite(+wal/shm), effects.log, evidence.txt
```

Crashes are simulated by forking the same binary as a child (`current_exe()`)
with `SPIKE_CRASH_AT=<point>`; the child calls `std::process::abort()` at the
selected point (SIGABRT, wait status 6). The parent recovers from the same
database file.

## Procedure

From the repository root:

```sh
# all six cases, offline build + per-case evidence
spikes/s0c/run_all.sh

# a single case
cd spikes/s0c && cargo run --release -- case1      # case1 .. case6

# offline build only (proves no network / no crates)
cd spikes/s0c && cargo build --release --offline
```

Each case prints a report beginning `=== case <name> ===`, one `PASS`/`FAIL`
line per check, a `VERDICT` line, and writes the same text to
`spikes/s0c/runs/<case>/evidence.txt`. `run_all.sh` exits non-zero if any case
fails. Run artifacts live only under `spikes/s0c/runs/` (gitignored) and under
`spikes/s0c/target/` (spike-local `.gitignore`); no temp directories are used.

## Crash matrix

All cases executed on the pinned versions above; full log at
`spikes/s0c/runs/run_all.log`. Each case's exact output is in its
`runs/<case>/evidence.txt`.

| # | Boundary / injection | Expected | Result | Evidence |
|---|---|---|---|---|
| 1 | crash **after transition commit, before dispatch** (`after_transition_commit`) | pending work recovered; state rebuilt; exactly one effect; duplicate returns recorded | PASS | child SIGABRT(6); `derived=building projected=building`; 1 unconsumed intent; 1 effect after recovery; duplicate `Recorded(...)`; still 1 effect |
| 2 | duplicate delivery **after activity completion** (no crash) | recorded result returned; effect not repeated; same op ID with different inputs rejected | PASS | first `Executed`, 1 effect; second `Recorded`, still 1 effect; conflict rejected: `op:e1:build reused with different inputs (existing=0c85d1734a1666a0, new=0c7bd1734a0e121b)` |
| 3 | restart → **state reconstructed from history events**; pending reconciled (`after_transition_commit` after verify transition) | replay matches projection; only verify pending; build effect not repeated; verify executed | PASS | child SIGABRT(6); `derived=verifying projected=verifying`; pending `[op:e1:verify]`; 1 effect before; 2 after; no pending left |
| 4 | crash **after simulated external effect, before completion record** (`after_effect_before_completion`) | effect survives; completion absent; recovery recognizes effect and completes without repeating it | PASS | child SIGABRT(6); effect present, 1 effect; op state `intended`; delivery `Reconciled(...)`; still 1 effect; op now `completed` |
| 5 | **kill latch survives restart**, blocks recovered dispatch (`after_latch_set`) | blocked while latched (no effect); executes after human resume | PASS | child SIGABRT(6); latch `active=true`; recovered dispatch `Blocked`; 0 effects; op incomplete; after resume `Executed`; 1 effect |
| 6 | **schema/execution version pinned, stored with results; incompatible parks** | result pins execution version; incompatible execution version parks; incompatible schema parks without effect | PASS | result `{... "execution_version":1}`; `schema_version=1`; forced `execution_version=99` → `Parked` + state `parked`; `schema_version=99` → `Parked`, effects `1 -> 1`; `compat` reports incompatible |

Matrix verdict: **6/6 PASS**.

### Fixture pack v0

The six cases in `spikes/s0c/src/cases.rs` are fixture pack v0 for B0 recovery
measurement. They are the protected baseline: B0 candidates must keep them
passing and must not edit them to weaken the checks. Run artifacts
(`runs/`) are disposable and regenerated on every run.

## How the semantics are implemented

- **Atomic commit** — `Executor::advance` builds one script and runs it through
  `sqlite3 -bail` on stdin: `UPDATE executions …; INSERT execution_events …;
  INSERT operations (state='intended') …; INSERT dispatch_intents …;` wrapped in
  `BEGIN IMMEDIATE … COMMIT`. Completion (`Executor::complete`) is a second
  transaction that updates the receipt, marks the intent consumed, and appends
  the event. Parking is analogous. No external I/O runs inside a transaction.
- **Stable operation IDs** — `op:<execution_id>:<kind>`, derived only from the
  execution and the logical step, so retries re-derive the same ID. Reuse with a
  different input hash is rejected; reuse with the same hash is idempotent.
- **Receipts and reconciliation** — a completed operation returns its stored
  `result_json`. A pending operation whose fake external effect is already
  present (a line in `effects.log`, synced with `sync_all`) is recognized and
  completed as `reconciled` without repeating the effect.
- **Restart** — `reconstruct_state` folds recorded events (no I/O) into a state
  and compares it with the `executions` projection; they must agree. Pending
  dispatch intents are then delivered.
- **Kill latch** — a single-row `kill_latch` table, checked before any effect
  and before recovered dispatch; set/cleared explicitly (human resume).
- **Versioning** — `meta.schema_version` gates all dispatch; a row's
  `execution_version` and every result carry the execution-definition version.
  Mismatch parks the operation and the execution.

## Handoff notes to B0 (durable execution bootstrap)

Recommended baseline to extract and harden behind `ExecutionApi`/`LedgerApi`:

1. **Tables to adopt as the seed schema** (`schema_version=1`): `meta`,
   `executions`, `operations`, `execution_events` (monotonic `sequence` per
   execution), `dispatch_intents` (`consumed` flag), `kill_latch`. The atomic
   commit API should accept a typed batch of {state change, event, receipt,
   intent} and commit whole-or-nothing, matching `Executor::advance`.
2. **Operation IDs**: keep `op:<execution_id>:<logical_step>` semantics and the
   input-hash conflict check. Generalize the step to the phase/operation
   roster, not just `build`/`verify`.
3. **Recovery order**: reconstruct state from events only (no I/O), verify
   against the projection, then reconcile pending intents. Do not perform
   effects during replay.
4. **Effect reconciliation contract**: handlers need an
   `effect_present(op_id)`-style probe. The fake here (append + `sync_all` to a
   log) stands in for Git refs, bd writes, provider calls, and merges; B0 needs
   real probes per activity and must park when the effect is ambiguous.
5. **Version gates**: keep `schema_version` + `execution_definition_version`
   stored with results; park incompatible state pending migration. Add the
   `user_version` pragma for the physical schema and a migration table.

Known gaps the spike deliberately does NOT cover — B0 must add and prove them:

- no resident worker loop, queue draining, or concurrency (cases call
  `deliver` explicitly, one operation at a time);
- no durable timers/deadlines, retry backoff, or approval waits;
- no lease generations/fencing, heartbeat, or orphan-process termination;
- no export/import, backup/restore, or migration mechanics;
- no real external effects; the effect is a local file;
- no bounded busy-retry policy beyond `busy_timeout`; each call is a separate
  `sqlite3` process, so per-call process overhead is not representative of an
  embedded driver;
- no measurement of idle memory/CPU or restart latency against metrics
  (evaluation criterion 8 remains open until B0).

## Deviations

- **SQLite access via the `sqlite3` CLI** rather than a Rust driver, by task
  instruction (zero-crate constraint). Durability pragmas (`WAL`,
  `synchronous=FULL`, `busy_timeout`) are applied per connection; the atomic
  commit is a single CLI invocation wrapping one `BEGIN IMMEDIATE … COMMIT`.
- **Crash injection uses `std::process::abort()`** in a forked copy of the same
  binary. This yields a genuine abnormal termination (SIGABRT) so no `Drop`
  handlers or buffered writes run after the injection point, which is the point
  of the test.
- **Cases 2 and 6 run single-process**; "restart" is exercised by calling the
  recovery path (`reconstruct_state` + `deliver`) on a reopened database. Cases
  1, 3, 4, and 5 use a real crashing child and a fresh parent process.
- No git commit/push and no bd writes were performed, per task constraints. The
  orchestrator owns those.

## Verdict

**Proceed.** The minimal serial executor's durability semantics — atomic
transition+receipt+intent commit, stable operation IDs, duplicate-delivery
receipt return, durable dispatch intents, event-history reconstruction,
effect-vs-completion reconciliation, restart-surviving kill latch, and version
gating with parking — are all demonstrated end-to-end over SQLite 3.51.0 with a
std-only, offline-buildable Rust spike. All six crash-matrix cases pass. The
seed schema and recovery order above are the handoff to B0; the listed gaps are
the next increments, not blockers.
