# B1.5 — task-queue extraction evidence

Bead: `agalma-0ht.6`. Design: `docs/b1-extraction.md` (D1–D5, D7). Depends on
`agalma-0ht.5` (B1.4 workspace extraction). HEAD at start: `78ab8e6`.

Goal: move `TaskQueueApi` (plus the reconcile projection operations the
conductor drives) behind a process (Unix-socket) boundary without changing any
contract schema or consumer (work/reconcile) logic, and prove replacement through
the declared API with dual-binding parity, staged rebind, rollback, and IPC
conformance.

## Deliverables

| # | Deliverable | Where |
|---|---|---|
| 1 | `TaskQueueClient` implementing `TaskQueueApi` + `TaskQueueOps` over `Client` | `crates/agalma-ipc/src/taskqueue_client.rs` |
| 1 | `TaskQueueOps` trait (frozen `TaskQueueApi` + `projection`/`set_status`/`remove_label`), `IssueProjection` wire DTO | `crates/agalma-ipc/src/taskqueue_client.rs` |
| 2 | `register_taskqueue` + `TASKQUEUE_METHODS` registration helper | `crates/agalma-ipc/src/taskqueue_server.rs` |
| 2 | Task-queue server bin hosting `BdTaskQueue` + `server_command` | `crates/agalma-taskqueue/src/bin/agalma-taskqueue-server.rs` + `src/server.rs` |
| 3 | `TaskQueueProcess` supervisor + `TaskQueueHandle` transport handle (reuses `spawn_supervised`/`stop_supervised`) | `crates/agalma-conductor/src/taskqueue_process.rs` |
| 3 | Construction sites switched to the transport handle | `crates/agalma-conductor/src/work.rs`, `lib.rs`, `reconcile.rs` |
| 4 | Parity / rebind / rollback / IPC-conformance tests | `crates/agalma-conductor/tests/taskqueue_process.rs` |
| 5 | This evidence doc + invariance proof | `docs/b1-taskqueue-evidence.md` |

### New interfaces

- `agalma_ipc`: `TaskQueueClient` (`connect`, `from_client`), `map_ipc_error`,
  `IssueProjection` (`has_label`, `has_comment`), param structs (`TaskCall`,
  `CommentParams`, `CloseParams`, `LabelParams`, `StatusParams`), `TaskQueueOps`
  (the server-hosted full surface); `register_taskqueue`, `TASKQUEUE_METHODS`,
  `taskqueue_server::ensure_all_handlers`.
- `agalma_taskqueue`: `impl TaskQueueOps for BdTaskQueue` (`src/lib.rs`);
  `server` module — `SERVER_BIN`, `server_command`, `build_server`,
  `build_server_with`, `serve`, `run_from_args`, `server_info`,
  `default_socket_for`, `API_NAME`, `API_VERSION`, `IMPL_NAME`, `IMPL_VERSION`,
  `BD_ENV`; re-exports `IssueProjection`/`TaskQueueOps` from `agalma-ipc`.
- `agalma_conductor::taskqueue_process`: `TaskQueueTransport` (`from_env`,
  `as_str`), `TRANSPORT_ENV`, `TASKQUEUE_SOCKET_NAME`, `taskqueue_server_spec`,
  `TaskQueueProcess` (`spawn`, `spawn_with_spec`, `socket`, `pid`, `client`,
  `stop`), `TaskQueueHandle` (`in_process`, `process`, `from_transport`).

### Why `TaskQueueOps`, not just `TaskQueueApi`

The conductor drives more than the frozen six-method `TaskQueueApi`: the
ledger-vs-projection reconcile (`reconcile.rs`) calls the adapter's
`projection`/`set_status`/`remove_label`. To keep work/reconcile logic unchanged
(D5) while carrying those operations across the boundary, `agalma-ipc` defines
`TaskQueueOps: TaskQueueApi` holding the full surface. `agalma-taskqueue`
implements it for the `bd`-backed `BdTaskQueue`; `TaskQueueClient` and the
conductor's `TaskQueueHandle` implement it over the transport (the handle
delegates to whichever backend is bound). This is an internal helper trait in
infrastructure; no contract schema changed. `IssueProjection` (previously
private to `agalma-taskqueue`) is now the wire DTO in `agalma-ipc`, re-exported
from `agalma-taskqueue` so existing paths keep resolving.

## Results

All commands from the repo root, `--offline`.

| Command | Observed |
|---|---|
| `cargo test --offline -p agalma-ipc` | exit 0; `39 passed` (unit, incl. taskqueue client) + `12 passed` (conformance) + doc-tests |
| `cargo test --offline -p agalma-taskqueue` | exit 0; `4 passed` (server module unit) + `0 passed` (server bin) + `16 passed` (bd integration) + doc-tests |
| `cargo test --offline -p agalma-conductor` | exit 0; 19 test binaries `test result: ok`, 0 failures (incl. `taskqueue_process`: `5 passed`, `architecture`: `4 passed`) |
| `cargo test --offline` (full workspace) | exit 0; every test binary `test result: ok`, 0 failures |

`tests/taskqueue_process.rs` cases:

| Test | What it proves |
|---|---|
| `shadow_parity_in_process_vs_process` | The same bd scenario (`ready_tasks`/`get`/`claim`/`comment`/`close_task`/`add_label`/`projection` over a five-issue scratch backlog) produces an identical, timestamp/id-free transcript in-process (`BdTaskQueue`) vs through the supervised task-queue server: ready set before and after, the skipped non-task, and each issue's projected status/labels/comments. Ordering is normalized (ids carry a per-store suffix) so parity is structural. |
| `staged_rebind_keeps_pinned_claim` | An in-flight claim made on server A stays pinned across a swap: the next operation runs on candidate server B, which sees the same `in_progress` status/assignee and the new comment; A remains alive and reports the same pinned claim. Mirrors the stateless-component rebind intent (D3: rebind between operations; the durable claim is the pinned artifact). |
| `rollback_keeps_prior_claim_and_server` | A candidate server is staged then rolled back (stopped, pid gone); the prior server and its persisted claim + comment survive, and the claimed task is not re-offered — no state loss. |
| `taskqueue_client_duplicate_id_replay_and_connection_loss` | Raw `Client`: a duplicate request id replays the recorded `claim` evidence; after the issue is closed the **same** id still returns the recorded receipt while a fresh id re-runs and is refused (`KnownFailure`) — proving the effect ran once and the task cannot be double-claimed; post-death call surfaces `Unavailable`/`UnknownOutcome`; a missing socket is `Unavailable`. |
| `taskqueue_server_child` | Child entry point (no-op under a normal run). |

The process tests spawn the task-queue server either from the real
`agalma-taskqueue-server` binary (when built, e.g. full-workspace runs) or by
re-exec'ing the test binary as the server
(`current_exe taskqueue_server_child --exact --nocapture`, socket via
`AGALMA_TEST_TASKQUEUE_SOCKET`, repo via `AGALMA_TEST_TASKQUEUE_REPO`), so a
package-scoped `cargo test -p agalma-conductor` run still exercises the real
process boundary. Both paths were run green (the real-binary path and, with the
binary moved aside, the re-exec path).

## Default transport decision

`TaskQueueTransport::from_env()` reads `AGALMA_TASKQUEUE_TRANSPORT`
(`in-process` default, `process` optional). The default remains **`in-process`**,
matching B1.2–B1.4. The construction sites now consult the transport factory:
`work::work` and `reconcile_command` build the queue through
`TaskQueueHandle::from_transport(&state_dir, &repo, TaskQueueTransport::from_env())`.
With the default the work/reconcile path is behaviourally unchanged; with
`AGALMA_TASKQUEUE_TRANSPORT=process` the sites spawn the supervised task-queue
server (socket `<state_dir>/taskqueue.sock`, serving `<repo>/.beads`) and bind a
`TaskQueueClient`.

## Conductor task-queue supervision

`TaskQueueProcess` spawns
`agalma-taskqueue-server --socket <state_dir>/taskqueue.sock --repo <repo>`
as a process-group leader, waits for `READY`, and connects `TaskQueueClient`s. It
reuses the exact `spawn_supervised`/`stop_supervised` helpers shared with the
harness/sandbox/workspace boundaries (one supervision implementation for every
boundary). The server targets the same `<repo>/.beads` store the in-process
backend used, so claims and projections are identical across bindings.

`TaskQueueHandle` holds either the in-process `BdTaskQueue` or a
`TaskQueueClient` plus the `Rc<TaskQueueProcess>` that keeps the server alive for
the handle's lifetime (stopped on drop). It exposes the full surface by
delegation, so the work/reconcile call bodies are unchanged — only the
constructed type differs.

## Deviations and design corrections (noted, not applied to `docs/b1-extraction.md`)

1. **`TaskQueueOps` carries the reconcile projection surface.** The design names
   `TaskQueueClient` as implementing "the same contract trait"; the conductor's
   reconcile also drives `projection`/`set_status`/`remove_label`, so the wire
   surface is the frozen `TaskQueueApi` plus those operations, unified by
   `TaskQueueOps`. No contract schema changed.
2. **`IssueProjection` moved to `agalma-ipc`.** The projection type was private
   to `agalma-taskqueue`; the transport needs one shared wire representation, so
   it now lives in infrastructure (`agalma-ipc`) with `has_label`/`has_comment`,
   and `agalma-taskqueue` re-exports it. `agalma-ipc` does not depend on any
   implementation crate.
3. **TaskQueue has no `Describe`/binding record.** `TaskQueueApi` has no
   `Describe`, so (as for the sandbox/workspace boundaries) the "staged rebind"
   is a supervised-server swap with the in-flight claim pinned to its claiming
   server (the bd store on disk is the durable artifact), rather than a
   `BindingRecord` swap (the harness boundary keeps the record-based rebind).
4. **The server takes `--repo <path>` (and optional `--bd`/`AGALMA_TASKQUEUE_BD`).**
   Unlike the socket-only sandbox/harness servers, the bd backend is rooted at a
   repository, so the server takes `--repo`; `run_from_args` parses `--socket`,
   `--repo`, and an optional `bd` executable override.
5. **No Tokio runtime in the task-queue server.** The backend shells out to `bd`
   via `std::process`, so the serial serve loop runs synchronously (mirroring the
   reference harness and workspace servers, not the Tokio-based sandbox/OpenCode
   servers).
6. **Unix socket path length.** The task-queue socket lives directly under the run
   state dir (`taskqueue.sock`) to stay within `SUN_LEN` (~104 bytes). Tests use
   short `target/test-runs/tq-<pid>-<n>/` run dirs.
7. **`Cargo.lock`.** Modified only for the new intra-workspace dependency edge
   (`agalma-taskqueue -> agalma-ipc`). No external crate was added.

## Consumer-invariance proof

`crates/agalma-conductor/src/work.rs` changes are limited to the import and the
queue type (`BdTaskQueue` → `TaskQueueHandle`) at four function parameters and
one construction site; every intake/claim/comment/close/label call, phase
dispatch, ordering, recovery, and activity body is unchanged. `lib.rs` and
`reconcile.rs` changes are limited to the queue construction/parameter type
(`TaskQueueHandle`) and the projection type (`agalma_ipc::IssueProjection`);
`reconcile_projection` compares exactly the same fields. The conductor tests
`fencing.rs`/`watchdog.rs` that construct a queue were updated to construct the
handle (`TaskQueueHandle::in_process`).

`git diff --name-only` (tracked) plus new files, all within the allowed set:

```
Cargo.lock                                                    # intra-workspace dep edge only
crates/agalma-conductor/src/lib.rs                            # module + construction site
crates/agalma-conductor/src/reconcile.rs                      # queue/projection type only
crates/agalma-conductor/src/work.rs                           # queue type + construction site
crates/agalma-conductor/src/taskqueue_process.rs              # new
crates/agalma-conductor/tests/fencing.rs                      # handle construction
crates/agalma-conductor/tests/watchdog.rs                     # handle construction
crates/agalma-conductor/tests/taskqueue_process.rs            # new
crates/agalma-ipc/src/lib.rs                                  # module + re-exports
crates/agalma-ipc/src/taskqueue_client.rs                     # new
crates/agalma-ipc/src/taskqueue_server.rs                     # new
crates/agalma-taskqueue/Cargo.toml                            # + agalma-ipc dep
crates/agalma-taskqueue/src/bd.rs                             # IssueProjection import (moved to ipc)
crates/agalma-taskqueue/src/lib.rs                            # TaskQueueOps impl + server module
crates/agalma-taskqueue/src/server.rs                         # new
crates/agalma-taskqueue/src/bin/agalma-taskqueue-server.rs    # new
docs/b1-taskqueue-evidence.md                                 # this file
```

No file under `crates/agalma-contracts/**` changed; no contract schema changed;
no `docs/b1-extraction.md` edit. The architecture lint is strict and green
(`cargo test -p agalma-conductor --test architecture`: `4 passed`): no
implementation crate depends on another, and no implementation source references
another implementation crate (`agalma-ipc` is infrastructure and may be depended
on).

# IPCX.5 — task-queue boundary migration to Connect (`agalma-ebg.6`)

Status: green (offline workspace build, ipc tests incl. new taskqueue Connect
conformance, conductor taskqueue-boundary tests over both wires, architecture).
Not committed (per worker instruction; peers active in tree, no git writes).

Design: `docs/ipc-connect-design.md` (D1–D7, D7 boundary #4). Pattern:
`agalma-ebg.3`/`agalma-ebg.4` (harness/sandbox boundaries).

## What changed

`TaskQueueApi`/`TaskQueueOps` typed client/server moved onto the Connect
transport; envelope semantics and consumer logic unchanged. Per-boundary wire
selector reused from `agalma-ipc::connect` (`Wire`, `WIRE_ENV`). The task-queue
process wire defaults to `Connect`; `AGALMA_IPC_WIRE=framed` restores framed for
parity runs.

| Concern | Where |
|---|---|
| `TaskQueueClient::connect_wire(socket, wire)` (+ framed `connect`/`from_client` retained) | `crates/agalma-ipc/src/taskqueue_client.rs` |
| `serve_taskqueue(server, socket, wire)` | `crates/agalma-ipc/src/taskqueue_server.rs` |
| `serve` reads `AGALMA_IPC_WIRE` (Connect default); `serve_wire` | `crates/agalma-taskqueue/src/server.rs` |
| `taskqueue_wire`, `TaskQueueProcess::wire`, `spawn_with_spec_wire`, child `WIRE_ENV` export, matching client | `crates/agalma-conductor/src/taskqueue_process.rs` |
| Wire parity + framed replay evidence | `crates/agalma-conductor/tests/taskqueue_process.rs` |
| Task-queue Connect conformance (new) | `crates/agalma-ipc/tests/taskqueue_connect.rs` |
| IPCX.5 evidence (this section) | `docs/b1-taskqueue-evidence.md` |

## Interfaces

- `agalma_ipc`: `TaskQueueClient::connect_wire`; `taskqueue_server::serve_taskqueue`.
- `agalma_taskqueue::server`: `serve_wire`; `serve` env-selects wire.
- `agalma_conductor::taskqueue_process`: `taskqueue_wire`,
  `TaskQueueProcess::wire`, `TaskQueueProcess::spawn_with_spec_wire`.

## Pins / deps

No new dependency. `connectrpc`/`buffa` stay confined to `agalma-ipc`
(architecture test green). No `Cargo.toml`/`Cargo.lock` change; builds `--offline`.

## Commands + results

| Command | Observed |
|---|---|
| `cargo build --offline --workspace` | exit 0 |
| `cargo test --offline -p agalma-ipc` | 48 + 14 + 9 + 4 + 3 + 2 + 2 (new `taskqueue_connect`) + 2 + 0 doc; 0 failures |
| `cargo test --offline -p agalma-taskqueue` | 25 unit + 0 bin + 21 integration; 0 failures |
| `cargo test --offline -p agalma-conductor --test taskqueue_process` | 8 passed, 0 failed |
| `cargo test --offline -p agalma-conductor --lib taskqueue_process` | 4 passed, 0 failed |
| `cargo test --offline -p agalma-conductor --test architecture` | 6 passed, 0 failed |

New/changed evidence tests:

- `taskqueue_api_over_connect_matches_framed` — the full operation surface
  (`ready_tasks`/`get`/`claim`/`comment`/`add_label`/`close_task` plus
  `projection`/`set_status`/`remove_label`) over a `register_taskqueue` server;
  framed and Connect produce an identical typed transcript.
- `taskqueue_connect_missing_socket_is_unavailable`.
- `wire_parity_framed_vs_connect_matches_in_process` — in-process vs framed vs
  Connect identical transcript for the same bd scenario.
- `shadow_parity_in_process_vs_process`, `staged_rebind_keeps_pinned_claim`,
  `rollback_keeps_prior_claim_and_server`,
  `undefer_release_survives_restart_and_matches_process_binding`,
  `process_binding_withholds_blocked_children` now run over Connect
  (`process.client()` on the stored wire).
- `taskqueue_client_duplicate_id_replay_and_connection_loss` stays framed and now
  spawns the server explicitly framed (raw `Client` conformance preserved).
- `taskqueue_process::tests::wire_defaults_to_connect_and_env_restores_framed`.

## Files (git diff --name-only, plus new)

```
crates/agalma-conductor/src/taskqueue_process.rs
crates/agalma-conductor/tests/taskqueue_process.rs
crates/agalma-ipc/src/taskqueue_client.rs
crates/agalma-ipc/src/taskqueue_server.rs
crates/agalma-ipc/tests/taskqueue_connect.rs            # new
crates/agalma-taskqueue/src/server.rs
docs/b1-taskqueue-evidence.md
```

`crates/agalma-contracts/**` unchanged; no contract schema changed. Untouched:
`work.rs`, `reconcile.rs`, `lib.rs` queue construction sites, `cli.rs`. Other
working-tree changes (`Cargo.toml`, `Cargo.lock`, `crates/agalma-conductor/Cargo.toml`,
`src/watch.rs`, `docs/tui.md`, workspace/ledger boundary files) belong to other
workers.

## Notes / deviations

1. `TaskQueueProcess::spawn`/`spawn_with_spec` keep their signatures (default to
   `taskqueue_wire()`), so existing callers are unedited;
   `spawn_with_spec_wire` is the explicit-wire entry point.
2. `TaskQueueApi` is `Describe`-free, so the "staged rebind" stays a
   supervised-server swap (B1.5 note 3), now over Connect.
3. The task-queue server still shells out to `bd` through `std::process`; the
   Connect branch's Tokio runtime is installed by `agalma-ipc::serve_wire`, so the
   impl crate needs no async runtime (unlike the sandbox boundary).
