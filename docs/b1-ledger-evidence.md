# B1.6 — ledger extraction evidence

Bead: `agalma-0ht.7`. Design: `docs/b1-extraction.md` (D1–D5, D7, D3 snapshot-file
rule). Depends on `agalma-0ht.6` (B1.5 task-queue extraction). HEAD at start:
`53ebe0c`.

Goal: move the durable-state component (`LedgerApi`) behind a process (Unix-socket)
boundary without changing any contract schema or executor/kernel logic, and prove
replacement through the declared API with dual-binding parity, a quiesced
export/import rebind, rollback, and IPC conformance. This is the durable-state
boundary: the server owns the database, enforces a single writer per state
generation, and transfers snapshots as files (never frames).

## Deliverables

| # | Deliverable | Where |
|---|---|---|
| 1 | `LedgerClient` implementing `LedgerApi` + `LedgerOps` over `Client` | `crates/agalma-ipc/src/ledger_client.rs` |
| 1 | `LedgerOps` trait (frozen `LedgerApi` + snapshot file primitives) | `crates/agalma-ipc/src/ledger_client.rs` |
| 2 | `register_ledger` + `LEDGER_METHODS` + `ensure_all_handlers` | `crates/agalma-ipc/src/ledger_server.rs` |
| 2 | Ledger server bin hosting `SqliteLedger` + `server_command` | `crates/agalma-ledger/src/bin/agalma-ledger-server.rs` + `src/server.rs` |
| 3 | Single-writer lock + snapshot file transfer (`export_snapshot_to`/`import_snapshot_from`) | `crates/agalma-ledger/src/server.rs`, `crates/agalma-ledger/src/lib.rs` |
| 4 | `LedgerProcess` supervisor + `LedgerHandle` transport handle | `crates/agalma-conductor/src/ledger_process.rs` |
| 4 | Construction sites switched to the transport handle | `crates/agalma-conductor/src/composition.rs`, `work.rs`, `lib.rs` |
| 5 | Parity / rebind / rollback / IPC-conformance tests | `crates/agalma-conductor/tests/ledger_process.rs` |
| 6 | This evidence doc + invariance proof | `docs/b1-ledger-evidence.md` |

### New interfaces

- `agalma_ipc`: `LedgerClient` (`connect(socket, snapshot_dir)`, `from_client`),
  `map_ipc_error`, `LedgerOps` (server-hosted full surface: `LedgerApi` +
  `export_snapshot_to`/`import_snapshot_from`), param structs (`CommitParams`,
  `ExecutionCall`, `OperationIdCall`, `OutcomeParams`, `LeaseCall`, `LatchParams`,
  `BindingParams`, `DecisionParams`, `DigestParams`, `KeyCall`, `MetaParams`,
  `TimerParams`, `TimerIdCall`, `DueTimersParams`, `DirectiveParams`,
  `DirectiveIdCall`, `PathParams`), `SNAPSHOT_EXPORT_FILE`/`SNAPSHOT_IMPORT_FILE`;
  `register_ledger`, `LEDGER_METHODS` (31 wire methods),
  `ledger_server::ensure_all_handlers`.
- `agalma_ledger`: `impl LedgerOps for SqliteLedger` (`src/lib.rs`); `server`
  module — `SERVER_BIN`, `server_command`, `build_server`, `serve`,
  `run_from_args`, `server_info`, `state_dir_from_args`, `default_socket_for`,
  `API_NAME`, `API_VERSION`, `IMPL_NAME`, `IMPL_VERSION`, `SOCKET_NAME`,
  `LOCK_NAME`, `DB_NAME`.
- `agalma_conductor::ledger_process`: `LedgerTransport` (`from_env`, `as_str`),
  `TRANSPORT_ENV`, `LEDGER_SOCKET_NAME`, `ledger_socket`, `ledger_server_spec`,
  `LedgerProcess` (`spawn`, `spawn_with_spec`, `socket`, `state_dir`, `pid`,
  `client`, `stop`), `LedgerHandle` (`in_process`, `process`, `from_transport`;
  `Clone`).

### Why `LedgerOps`, not just `LedgerApi`

The frozen `LedgerApi` carries `export_state`/`import_state` as in-memory
`StateSnapshot` values. The durable-state snapshot can exceed the 16 MiB frame
cap, so D3 requires snapshot transfer **via files under the run state dir, not
frames**. `LedgerOps: LedgerApi` adds the path-based primitives the server hosts
(`export_snapshot_to`/`import_snapshot_from`); `LedgerClient` implements the
trait `export_state`/`import_state` over them (server writes/reads the file, the
client transfers only the path and reads/parses the file locally on the same
host). `agalma-ledger` implements `LedgerOps` for `SqliteLedger`; the conductor's
`LedgerHandle` implements it by delegation. This is an internal helper trait in
infrastructure; no contract schema changed.

## Results

All commands from the repo root, `--offline`.

| Command | Observed |
|---|---|
| `cargo test --offline -p agalma-ipc` | exit 0; `43 passed` (unit, incl. ledger client) + `12 passed` (conformance) + doc-tests |
| `cargo test --offline -p agalma-ledger` | exit 0; `26 passed` (unit, incl. server single-writer lock) + `0 passed` (server bin) + doc-tests |
| `cargo test --offline -p agalma-kernel` | exit 0; every test binary `test result: ok`, 0 failures |
| `cargo test --offline -p agalma-execution` | exit 0; `11 passed`, 0 failures |
| `cargo test --offline -p agalma-conductor` | exit 0; 18 test binaries `test result: ok`, 0 failures (incl. `ledger_process`: `6 passed`, `architecture`: `4 passed`) |
| `cargo test --offline` (full workspace) | exit 0; 67 `test result: ok` lines, 0 failures |

`tests/ledger_process.rs` cases:

| Test | What it proves |
|---|---|
| `shadow_parity_in_process_vs_process` | The same durable-state scenario (lease → atomic commit of state+event+intent → recorded receipt → inspect → binding → kernel meta → timer → export) produces an identical, timing/id-free transcript in-process (`SqliteLedger`) vs through the supervised ledger server: lease, commit revisions, pending-work counts, execution/event/binding/timer counts, kernel meta, schema version, and the **normalized exported snapshot** (wall-clock fields zeroed). |
| `quiesced_rebind_preserves_pinned_operation` | D3 step 3: server A owns a pinned (unconsumed) dispatch intent; the conductor drains A, exports the canonical snapshot to a file, and starts a fresh server B from the snapshot. The pinned operation's lease generation and intent id survive; recording its receipt settles it on the new binding (pending work → 0, receipt recorded). A's pid is gone; B serves. |
| `rollback_keeps_prior_state_and_server` | A candidate server B is staged on its own state dir, then rolled back (stopped, pid gone); the prior server A and its persisted state survive, and the next operation (receipt commit) succeeds on the restored binding — no state loss. |
| `ledger_client_duplicate_id_replay_and_connection_loss` | Raw `Client`: a duplicate request id replays the recorded `commit` receipt (the event count stays 1 — no double effect); after the server dies the next call surfaces `Unavailable`/`UnknownOutcome`; the state is reconciled through the inspection APIs on a restarted server; an incompatible snapshot import (bad `format` or `schema_version`) is refused with `Conflict`; a missing socket is `Unavailable`; the path-based snapshot export writes the file server-side. |
| `transport_defaults_to_in_process` | `LedgerTransport::from_env` defaults to `in-process`; `as_str` selectors are stable. |
| `ledger_server_child` | Child entry point (no-op under a normal run). |

The process tests spawn the ledger server either from the real
`agalma-ledger-server` binary (when built, e.g. full-workspace runs) or by
re-exec'ing the test binary as the server
(`current_exe ledger_server_child --exact --nocapture`, socket via
`AGALMA_TEST_LEDGER_SOCKET`, state dir via `AGALMA_TEST_LEDGER_STATE_DIR`), so a
package-scoped `cargo test -p agalma-conductor` run still exercises the real
process boundary. Both paths were run green (the real-binary path and, with the
binary moved aside, the re-exec path).

## Default transport decision

`LedgerTransport::from_env()` reads `AGALMA_LEDGER_TRANSPORT` (`in-process`
default, `process` optional). The default remains **`in-process`**, matching
B1.2–B1.5. The construction sites now consult the transport factory:
`Composition::build` and `work::work` build the executor's ledger through
`LedgerHandle::from_transport(&state_dir, LedgerTransport::from_env())`. With the
default the run/work path is behaviourally unchanged; with
`AGALMA_LEDGER_TRANSPORT=process` the sites spawn the supervised ledger server
(socket `<state_dir>/ledger.sock`, owning `<state_dir>/ledger.sqlite`) and bind a
`LedgerClient`. The kernel/executor construction (`Executor<L: LedgerApi>`) is
unchanged: it now receives `Executor<LedgerHandle>` at the conductor seam only.

## Single-writer rule (D3)

The ledger server is the single writer for one state generation. On start it
acquires `<state_dir>/ledger-server.lock` (pid written to the file); a second
server for the same state dir **refuses to start** while a live pid holds the
lock, and reclaims a stale lock (dead pid). The lock is released on process exit
(killed) or removed on clean return. The rule: **at most one ledger server (one
writer) owns a state dir at a time**; `export_state`/`import_state` run only while
quiesced, and rebinding moves a snapshot file between servers rather than sharing
a database.

## Snapshot file transfer (D3)

`export_snapshot_to`/`import_state_from` write/read the canonical JSON snapshot
(`StateSnapshot::to_canonical_json`) at a caller-supplied path under the run
state dir. Only the path crosses the wire; the snapshot payload never becomes a
frame. `LedgerApi::export_state`/`import_state` on `LedgerClient` are thin
wrappers: the server writes/reads the file, the client reads/writes it locally on
the same host and (de)serializes.

## Deviations and design corrections (noted, not applied to `docs/b1-extraction.md`)

1. **`LedgerOps` carries the snapshot file primitives.** The design names
   `LedgerClient` as implementing "the same contract trait"; D3 requires snapshot
   transfer by file, so the wire surface is the frozen `LedgerApi` plus
   `export_snapshot_to`/`import_snapshot_from`, unified by `LedgerOps`. No
   contract schema changed.
2. **The handle is cloneable and shared.** Unlike the stateless component
   handles, `LedgerHandle` is `Clone` over an `Rc`-shared backend so every
   per-task executor in a work loop uses **one** durable-state owner (single
   writer per state generation). The supervised server lives until the last
   clone drops.
3. **`reconcile.rs` generalized over `LedgerApi`.** `reconcile_projection` and
   its helpers now take `&L where L: LedgerApi + ?Sized` instead of
   `&SqliteLedger`, so the same call accepts either the in-process ledger or the
   transport handle. The comparison/rewrite logic is unchanged (the call bodies
   are byte-for-byte the same methods).
4. **Auxiliary direct connections remain.** The executor's ledger — the
   extracted boundary — is bound through `LedgerHandle`. A few short-lived
   conductor helpers (`digest::record`, `provider_auth::record_audit`,
   `routing::resolve_route`'s decider, the `phases.rs` activity event writers,
   and the read-only `status`/`digest`/`reconcile`/`resume` CLI subcommands)
   still open their own `SqliteLedger` connection for a single operation. Under
   `process` transport these are secondary SQLite (WAL) connections to the same
   file; the durable-state owner and the export/import generation are still the
   server (guarded by the lock), and the rebind test quiesces the conductor
   before exporting. Routing every auxiliary access through the boundary is
   deferred to keep the diff at the construction sites (D5).
5. **`LedgerApi` has no `Describe`/binding record.** As for the sandbox/workspace/
   task-queue boundaries, the "staged rebind" is a supervised-server swap with
   the in-flight pinned dispatch intent preserved across a snapshot
   export/import, rather than a `BindingRecord` swap (the harness boundary keeps
   the record-based rebind).
6. **The server takes `--socket <path>` and `--state-dir <path>`.** The ledger
   backend is rooted at a state dir (DB + snapshot files + lock), so the server
   takes both; `run_from_args` parses them.
7. **No Tokio runtime in the ledger server.** The backend is `rusqlite`
   (synchronous), so the serial serve loop runs synchronously (mirroring the
   reference harness, workspace, and task-queue servers).
8. **Unix socket path length.** The ledger socket lives directly under the run
   state dir (`ledger.sock`) to stay within `SUN_LEN` (~104 bytes). Tests use
   short `target/test-runs/lp-<pid>-<n>/` run dirs.
9. **`Cargo.lock`.** Modified only for the new intra-workspace dependency edge
   (`agalma-ledger -> agalma-ipc`). No external crate was added.

## Consumer-invariance proof

`crates/agalma-conductor/src/composition.rs` changes are limited to the ledger
type (`SqliteLedger` → `LedgerHandle`), the handle construction
(`LedgerHandle::from_transport`), and the `Composition.executor` type
(`Executor<LedgerHandle>`); every phase activity, prompt materialization, and
recovery call is unchanged. `work.rs` changes are limited to the ledger
construction site, the executor/decider parameter types
(`Executor<LedgerHandle>`, `StaticDecider<LedgerHandle>`), the boot schema check
and recovery read now going through the handle, and threading `&LedgerHandle`
into `run_task`/`recover_inflight`/`build_executor`; every intake/claim/comment/
close/label call, phase dispatch, ordering, and recovery body is unchanged.
`lib.rs` changes are limited to `resume_or_start`/`drive` parameter types, the
`binding_label` generic parameter, and the signal-latch handler (which now opens
the bound connection instead of a fixed file). `reconcile.rs` is generalized over
`LedgerApi` with identical logic. No `crates/agalma-kernel/**` or
`crates/agalma-execution/**` source changed: both are already generic over
`LedgerApi`, so the seam wiring is purely at the conductor construction sites.

`git diff --name-only` (tracked) plus new files, all within the allowed set:

```
Cargo.lock                                                    # intra-workspace dep edge only
crates/agalma-conductor/src/composition.rs                    # ledger type + handle construction
crates/agalma-conductor/src/lib.rs                            # executor type + signal-latch handler
crates/agalma-conductor/src/ledger_process.rs                 # new
crates/agalma-conductor/src/reconcile.rs                      # generic over LedgerApi (logic unchanged)
crates/agalma-conductor/src/work.rs                           # ledger type + handle construction
crates/agalma-conductor/tests/ledger_process.rs               # new
crates/agalma-ipc/src/lib.rs                                  # module + re-exports
crates/agalma-ipc/src/ledger_client.rs                        # new
crates/agalma-ipc/src/ledger_server.rs                        # new
crates/agalma-ledger/Cargo.toml                               # + agalma-ipc dep
crates/agalma-ledger/src/lib.rs                               # LedgerOps impl + server module
crates/agalma-ledger/src/server.rs                            # new
crates/agalma-ledger/src/bin/agalma-ledger-server.rs          # new
docs/b1-ledger-evidence.md                                    # this file
```

No file under `crates/agalma-contracts/**` changed; no contract schema changed;
no `docs/b1-extraction.md` edit. The architecture lint is strict and green
(`cargo test -p agalma-conductor --test architecture`: `4 passed`): no
implementation crate depends on another, and no implementation source references
another implementation crate (`agalma-ipc` is infrastructure and may be depended
on).
