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
4. **Auxiliary direct connections remain (accepted deviation; B1-F2
   `agalma-1o3`).** The executor's ledger — the extracted boundary — is bound
   through `LedgerHandle`. A few short-lived conductor helpers
   (`digest::record`, `provider_auth::record_audit`, `routing::resolve_route`'s
   decider, the `phases.rs` activity event writers, and the read-only
   `status`/`digest`/`reconcile`/`resume` CLI subcommands) still open their own
   `SqliteLedger` connection for a single operation. Under `process` transport
   these are secondary SQLite (WAL) connections to the same file; the
   durable-state owner and the export/import generation are still the server
   (guarded by the lock), and the rebind test quiesces the conductor before
   exporting.

   B1-F2 revisited this and converged what is reachable, with the remainder
   accepted deliberately because routing them through the boundary is impossible
   **without a redesign** that is out of this task's scope:

   - **Why a fresh process connection cannot be used.** The ledger server serves
     **one connection at a time** — `Server::accept_loop` accepts a connection and
     runs `serve_connection` to completion (until the peer disconnects) before it
     returns to `accept`. The conductor's bound `LedgerHandle` holds its client
     connection open for the whole run, so any auxiliary helper that opened its
     *own* `LedgerClient` would queue in the listener backlog and **block
     forever**. This is pinned by
     `tests/ledger_process.rs::second_connection_is_not_served_while_the_primary_is_held`.
   - **Why the shared handle is not reachable.** The only correct route is to
     reuse the *same* `LedgerHandle` (shared `Rc` backend, single connection).
     That handle is owned by the executor (`work.rs`) and by the activity set
     (`phases.rs`), neither of which passes it to the auxiliary helpers: the
     `Activity` trait (`agalma_execution`) receives only the
     `OperationContext`, and threading the handle through the executor/activity
     seam, the `activities(...)` constructor, and the `work.rs` call sites is a
     kernel/execution + `work.rs` change outside this bead's file scope.
   - **Why the read-only CLI subcommands stay direct.** They run as standalone
     processes; under `process` transport the conductor (and its `ledger-server`
     lock) is usually live, so spawning a server fails and a client connection
     would deadlock. A WAL read is the correct, non-blocking access there.
   - **Converged in B1-F2.** `provider_auth::record_audit_via` takes a bound
     ledger (no state-dir argument, so it structurally cannot open a secondary
     connection) and the in-scope `Composition::build` call site now passes its
     already-bound `LedgerHandle`. The `record_audit(state_dir, …)` wrapper is
     retained only for the off-limits `work.rs` call site. A bound-handle routing
     proof lives in `provider_auth::tests::record_audit_via_routes_through_the_bound_ledger`.

   Net: one fewer secondary connection on the M0 (`run`) path; the work-loop and
   CLI accesses remain documented, secondary WAL reads/writes with the accepted
   rationale above. The goal here is single-writer *clarity* (each access is now
   either on the handle or explicitly named here), not correctness today.
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

## B1-F2 — auxiliary ledger connections (`agalma-1o3`)

Follow-up to deviation 4. Scope: `crates/agalma-conductor` only
(`digest.rs`, `provider_auth.rs`, `routing.rs`, `phases.rs`,
`ledger_process.rs`, minimal `lib.rs` wiring, co-located tests). The work loop
(`work.rs`), the read-only TUI (`watch.rs`, `status.rs`), and the review/sandbox
modules are owned by other workers and were not touched.

### Converged
- `provider_auth::record_audit_via(ledger: &mut dyn LedgerApi, audit)` — the audit
  writer now takes the bound ledger and has **no state-dir argument**, so it
  cannot open a secondary connection. `record_audit(state_dir, …)` remains as a
  thin wrapper for callers that do not hold the handle.
- `Composition::build` (`composition.rs`) passes its already-bound `LedgerHandle`
  to `record_audit_via`, so the M0 `run` path records the provider-auth audit on
  the conductor's single ledger connection. In-process behaviour is unchanged
  (same backend, one fewer connection); under `process` transport the audit no
  longer opens a second SQLite connection to the server's database.

### Documented deviation (rationale in deviation 4 above)
- `digest::record` (caller: `work.rs`), `routing::resolve_route`'s decider
  (callers: `work.rs`, `review.rs`, `sweep.rs`, `concierge.rs`), the `phases.rs`
  activity event writers, and the read-only `digest`/`reconcile`/`resume` CLI
  subcommands keep direct secondary WAL connections. Routing them through the
  boundary needs the shared `LedgerHandle`, which is not threaded to these
  helpers, and a fresh process connection would deadlock against the serial
  ledger server. Both facts are structural; removing them is a redesign
  (executor/activity seam + `work.rs` + CLI), out of scope here.

### Tests
- `provider_auth::tests::record_audit_via_routes_through_the_bound_ledger` — the
  audit writer records through the passed handle (no path ⇒ no secondary
  connection).
- `tests/ledger_process.rs::process_transport_routes_provider_auth_audit_through_the_handle`
  — under the real process boundary, `record_audit_via` writes through the bound
  `LedgerHandle`/ledger server and reads the event back through the same handle.
- `tests/ledger_process.rs::second_connection_is_not_served_while_the_primary_is_held`
  — pins the serial single-connection server behaviour that makes a
  fresh-connection route impossible (the deviation rationale).

### Results

| Command | Observed |
|---|---|
| `cargo build --offline --workspace` | exit 0 |
| `cargo test --offline -p agalma-conductor` | exit 0, all green (incl. the two tests above) |

## IPCX.6 — ledger boundary migration to Connect (`agalma-ebg.7`)

Design: `docs/ipc-connect-design.md` (IPCX.6, D7 boundary #5). Pattern:
`agalma-ebg.4` (sandbox boundary, commit `dd18375`) and `agalma-ebg.3` (harness
boundary, commit `1c1d89c`). HEAD at start: `4628801`.

The typed `LedgerApi`/`LedgerOps` client and the SQLite server moved onto the
Connect transport; envelope semantics and consumer logic are unchanged. The
ledger process wire defaults to `Connect`; `AGALMA_IPC_WIRE=framed` restores the
B1 framed transport for parity runs. **Snapshot transfer stays file-based on
both wires (D3): no behaviour change there** — only the path crosses the wire,
never the payload.

| Concern | Where |
|---|---|
| `LedgerClient::connect_wire(socket, snapshot_dir, wire)` (+ framed `connect`/`from_client` retained); `Backend::{Framed,Connect}` | `crates/agalma-ipc/src/ledger_client.rs` |
| `serve_ledger(server, socket, wire)` | `crates/agalma-ipc/src/ledger_server.rs` |
| `LEDGER_METHODS` unchanged (33, incl. `record_control_receipt`/`control_receipts` from `agalma-3kr.6`) | `crates/agalma-ipc/src/ledger_server.rs` |
| `serve` reads `AGALMA_IPC_WIRE` (Connect default); `serve_wire` | `crates/agalma-ledger/src/server.rs` |
| `ledger_wire`, `LedgerProcess::wire`, `spawn_with_spec_wire`, child `WIRE_ENV` export, matching client | `crates/agalma-conductor/src/ledger_process.rs` |
| Wire parity + framed replay evidence | `crates/agalma-conductor/tests/ledger_process.rs` |
| Ledger Connect conformance (new) | `crates/agalma-ipc/tests/ledger_connect.rs` |
| IPCX.6 evidence section (this) | `docs/b1-ledger-evidence.md` |

### Interfaces

- `agalma_ipc`: `LedgerClient::connect_wire`; `ledger_server::serve_ledger`.
- `agalma_ledger::server`: `serve_wire`; `serve` env-selects wire.
- `agalma_conductor::ledger_process`: `ledger_wire`, `LedgerProcess::wire`,
  `LedgerProcess::spawn_with_spec_wire`.

### Pins / deps

No new dependency. `connectrpc`/`buffa` stay confined to `agalma-ipc`
(architecture test green, `connect_stack_is_confined_to_agalma_ipc`). No
`Cargo.toml`/`Cargo.lock` change; builds `--offline`.

### Commands + results

| Command | Observed |
|---|---|
| `cargo build --offline --workspace` | exit 0 |
| `cargo test --offline -p agalma-ipc -p agalma-ledger` | 48 + 14 + 9 + 4 + 3 (new `ledger_connect`) + 2 + 2 + 0 doc (ipc); 30 + 0 (ledger); 0 failures |
| `cargo test --offline -p agalma-conductor --test ledger_process` | 10 passed, 0 failed |
| `cargo test --offline -p agalma-conductor --lib ledger_process` | 4 passed, 0 failed |
| `cargo test --offline -p agalma-conductor --test architecture` | 6 passed, 0 failed |

New/changed evidence tests:

- `ledger_api_over_connect_matches_framed` (new, `crates/agalma-ipc/tests/ledger_connect.rs`)
  — a `register_ledger` server hosting a deterministic stub is driven through
  lease / kernel-meta / schema-version / control-receipt / file-based snapshot
  export over both wires; framed and Connect produce an identical typed
  transcript.
- `ledger_connect_missing_socket_is_unavailable` (new) — a missing socket maps to
  `Unavailable` on the Connect client.
- `ledger_methods_include_control_receipt_ops` (new) — `LEDGER_METHODS` stays at
  33 and includes the `agalma-3kr.6` `record_control_receipt`/`control_receipts`
  ops and the two snapshot file primitives.
- `wire_parity_framed_vs_connect_matches_in_process`
  (`tests/ledger_process.rs`) — in-process `SqliteLedger` vs framed process vs
  Connect process produce an identical durable-state transcript (lease, commit
  revisions, pending work, receipts, kernel meta, timers, schema version, and the
  normalized exported snapshot).
- `quiesced_rebind_preserves_pinned_operation`, `rollback_keeps_prior_state_and_server`,
  `shadow_parity_in_process_vs_process`, `control_receipts_queryable_over_process_boundary`,
  `process_transport_routes_provider_auth_audit_through_the_handle` now run over
  Connect by default (`spawn_server` → `ledger_wire()`).
- `ledger_client_duplicate_id_replay_and_connection_loss` and
  `second_connection_is_not_served_while_the_primary_is_held` stay framed (raw
  `Client`/`UnixStream` conformance; the latter pins the framed serial
  single-connection server — the B1-F2 deviation rationale).
- `ledger_process::tests::wire_defaults_to_connect_and_env_restores_framed`.

### Notes / deviations

1. Snapshot paths are unchanged: `SNAPSHOT_EXPORT_FILE`/`SNAPSHOT_IMPORT_FILE`
   under the run state dir, written server-side / read client-side. The `D3`
   file-transfer contract is wire-independent.
2. The `agalma-3kr.6` control-receipt ops (`record_control_receipt`,
   `control_receipts`) are part of the frozen 33-method surface and are proven
   over both wires by the conformance + parity tests; no `LEDGER_METHODS` change.
3. `serve_wire` installs a multithread Tokio runtime for both wires (the framed
   branch runs the serial accept loop under `block_on`); the `rusqlite` backend
   stays synchronous. This supersedes the B1.6 note "no Tokio runtime in the
   ledger server" — the runtime is a transport host, not a backend change.
4. The B1-F2 auxiliary-connection deviation (deviation 4 above) is a *framed*
   server property; Connect multiplexes, but the auxiliary helpers
   (`digest::record`, `routing::resolve_route`, `phases.rs` writers, read-only CLI
   subcommands) are unchanged — IPCX.6 is a transport migration, not a
   convergence pass. `second_connection_is_not_served_while_the_primary_is_held`
   is pinned to the framed wire accordingly.
5. The `LedgerOps` split (`export_state_to`/`import_state_from`) is unchanged;
   `export_state`/`import_state` on the client remain thin wrappers over the two
   file primitives.

### Files (git diff --name-only, plus new)

```
crates/agalma-conductor/src/ledger_process.rs
crates/agalma-conductor/tests/ledger_process.rs
crates/agalma-ipc/src/ledger_client.rs
crates/agalma-ipc/src/ledger_server.rs
crates/agalma-ipc/src/lib.rs                                # + serve_ledger re-export
crates/agalma-ipc/tests/ledger_connect.rs                    # new
crates/agalma-ledger/src/server.rs
docs/b1-ledger-evidence.md                                   # this section
```

`crates/agalma-contracts/**` unchanged; no contract schema changed. Untouched:
`composition.rs`, `work.rs`, `lib.rs` (conductor), harness/workspace/taskqueue
boundary files.


