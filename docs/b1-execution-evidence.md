# B1.7 — execution/kernel extraction evidence

Bead: `agalma-0ht.8`. Design: `docs/b1-extraction.md` (D1–D5, D7; execution
boundary note under "Risks / open items"). Depends on `agalma-0ht.7` (B1.6
ledger extraction). HEAD at start: `adf21d0`.

Goal: move the `ExecutionApi` boundary behind a process (Unix-socket) boundary
without changing any contract schema or consumer logic, and prove replacement
through the declared API with dual-binding parity, a staged rebind with an
in-flight pinned operation, rollback, and IPC conformance.

## Boundary resolution (done first)

The design record flagged: "`KernelEngine` does not implement `ExecutionApi`
today (only `Executor<L>` does); B1.7 must resolve the actual replaceable
boundary before extraction." Resolution:

- The conductor's current execution service is the M1 seed
  `Executor<L>` (`crates/agalma-execution/src/executor.rs`). It hosts
  `Vec<(ExecutionPhase, Box<dyn Activity>)>` — activity closures built from
  conductor code. Those closures cannot cross a process boundary without
  re-architecting phases, so **moving the seed executor out is out of scope**.
- The replaceable boundary with real pressure is the **B0 durable kernel**:
  `KernelEngine<L: LedgerApi>` (`crates/agalma-kernel/src/engine.rs`) is pure
  data-in/data-out over `LedgerApi` (`apply` / `pending_dispatch` / `receipt` /
  timers / directives / leases) with a `Box<dyn SandboxApi + Send>` seam
  (`with_sandbox`). It is the candidate replacement for the seed executor and is
  process-ready in shape.
- **B1.7 therefore extracts `ExecutionApi` for the kernel candidate**: a server
  bin hosting `KernelEngine<LedgerClient>` (+ `SandboxClient` seam) and an
  `ExecutionClient` implementing `ExecutionApi`.
- **Default conductor execution remains the seed executor in-process.** The
  kernel process binding is candidate-only: selectable
  (`AGALMA_EXECUTION_TRANSPORT=process`) and exercised by tests; the production
  path is unchanged.

### Faithfulness and gaps (`ExecutionApi` → kernel)

The kernel separates the **pure transition** (`KernelEngine::apply`) from the
**external effect dispatch** (`run_activity`/`recover_pending`, which call a
registered `ActivityHandler`). Handlers are consumer-built Rust closures that
cannot be hosted generically behind the boundary, so:

| `ExecutionApi` method | Mapping | Result |
|---|---|---|
| `start(task)` | derive next free `ExecutionId::derive(task, n)`; `apply(Command::Start)` | faithful |
| `cancel(exec)` | `KernelEngine::cancel` (`Command::Cancel`) | faithful |
| `status(exec)` | project `KernelEngine::load` + `pending_dispatch` onto `ExecutionStatus` | faithful |
| `step(exec)` | needs effect dispatch through a handler registry | `UnsupportedCapability` (bare adapter) / **faithful via handler-registry protocol (B1-F1)** |
| `recover()` | needs handler-driven effect reconciliation | `UnsupportedCapability` (bare adapter) / **faithful via handler-registry protocol (B1-F1)** |

Flattened cases (dispatch/recovery) are covered at the engine-op level through
`ExecutionOps` (the same shape as the `LedgerOps`/`TaskQueueOps` precedent), and
the adapter gap is asserted directly
(`execution_api_adapter_reports_unsupported_capability`). B1-F1 (`agalma-5de`)
closes the gap for callers that attach a handler registry (see below); the bare
`KernelEngine` adapter still reports `UnsupportedCapability` because it hosts no
handlers.

## Deliverables

| # | Deliverable | Where |
|---|---|---|
| 1 | `ExecutionClient` implementing `ExecutionApi` | `crates/agalma-ipc/src/execution_client.rs` |
| 1 | `ExecutionOps` (frozen `ExecutionApi` + kernel engine surface) | `crates/agalma-ipc/src/execution_client.rs` |
| 2 | `register_execution` + `EXECUTION_METHODS` + `ensure_all_handlers` | `crates/agalma-ipc/src/execution_server.rs` |
| 2 | Kernel server bin hosting `KernelEngine<LedgerClient>` + `server_command` | `crates/agalma-kernel/src/bin/agalma-kernel-server.rs` + `src/server.rs` |
| 2 | `impl ExecutionApi`/`impl ExecutionOps for KernelEngine<L>` | `crates/agalma-kernel/src/exec_api.rs` |
| 3 | Candidate transport wiring (`ExecutionTransport`, `ExecutionProcess`, spec) | `crates/agalma-conductor/src/execution_process.rs` |
| 4 | Parity / rebind / rollback / IPC-conformance tests | `crates/agalma-conductor/tests/execution_process.rs` (+ `crates/agalma-kernel/tests/exec_api.rs`) |
| 5 | This evidence doc + invariance proof | `docs/b1-execution-evidence.md` |

### New interfaces

- `agalma_ipc`: `ExecutionClient` (`connect`, `from_client`), `ExecutionOps`
  (server-hosted surface: `ExecutionApi` + `apply`/`load_state`/`pending_dispatch`/
  `receipt`/`fire_due_timers`/`schedule_timer`/`due_timers`/`begin_lease`/
  `lease_generation`/`reclaim`/`submit_directive`/`export_snapshot_to`/
  `import_snapshot_from`), `map_ipc_error`, param structs (`StartCall`,
  `ExecIdCall`, `KernelCall`, `ReceiptCall`, `NowCall`, `TimerExecParams`,
  `LeaseExecCall`, `ReclaimCall`, `DirectiveExecCall`, `SnapshotPathCall`);
  `register_execution`, `EXECUTION_METHODS` (18 wire methods),
  `execution_server::ensure_all_handlers`.
- `agalma_kernel`: `exec_api` module (`impl ExecutionApi`/`ExecutionOps for
  KernelEngine<L>`); `server` module — `SERVER_BIN`, `server_command`,
  `build_server`, `serve`, `run_from_args`, `state_dir_from_args`,
  `default_socket_for`, `server_info`, `API_NAME`, `API_VERSION`, `IMPL_NAME`,
  `IMPL_VERSION`, `SOCKET_NAME`, `LEDGER_SOCKET_ENV`, `SANDBOX_SOCKET_ENV`.
- `agalma_conductor::execution_process`: `ExecutionTransport` (`from_env`,
  `as_str`), `TRANSPORT_ENV`, `EXECUTION_SOCKET_NAME`, `execution_socket`,
  `execution_server_spec`, `ExecutionProcess` (`spawn`, `spawn_with_spec`,
  `socket`, `state_dir`, `pid`, `client`, `stop`).

## Results

All commands from the repo root, `--offline`.

| Command | Observed |
|---|---|
| `cargo test --offline -p agalma-ipc` | exit 0; `48 passed` (unit, incl. execution client) + `12 passed` (conformance) + doc-tests |
| `cargo test --offline -p agalma-kernel` | exit 0; every test binary `test result: ok` (incl. `exec_api`: `4 passed`), 0 failures |
| `cargo test --offline -p agalma-conductor` | exit 0; every test binary `test result: ok` (incl. `execution_process`: `8 passed`, `architecture`: `4 passed`), 0 failures |
| `cargo test --offline` (full workspace) | exit 0; 70 `test result: ok` lines, 0 failures |

`tests/execution_process.rs` cases:

| Test | What it proves |
|---|---|
| `shadow_parity_in_process_vs_process` | The same engine-op scenario (lease → start → announce/dispatch a build op → complete it → schedule/fire a timer → submit a directive → cancel) produces identical `ApplyOutcome`s, receipt kinds, pending-work counts and op ids, timer-fire flags, status, and the **normalized exported snapshot** (wall-clock fields zeroed) in-process (`KernelEngine<SqliteLedger>`) vs through the boundary (`ExecutionClient` → `KernelEngine<LedgerClient>` → ledger server). |
| `rebind_preserves_in_flight_pinned_operation` | Case 9 across the process boundary: a pinned (unconsumed) dispatch intent is exported to a snapshot file, the binding is drained/stopped, and a fresh process binding imports the snapshot. The pinned operation id and its binding pin survive; completing it on the new binding settles it (pending → 0, receipt recorded). |
| `rollback_keeps_prior_binding_and_state` | A candidate process binding is staged on its own state dir, then rolled back (stopped, pid gone). The prior binding survives with its pending work and serves the next operation (no state loss). |
| `execution_client_duplicate_id_replay_and_connection_loss` | Raw `Client`: a duplicate request id replays the recorded `begin_lease` result (lease generation stays 1 — no double effect); after the server dies the next call surfaces `Unavailable`/`UnknownOutcome`. |
| `execution_api_adapter_reports_unsupported_capability` | `start`/`status` map faithfully through the adapter; `step`/`recover` return `UnsupportedCapability`; a missing socket is `Unavailable`. |
| `transport_defaults_to_in_process` | `ExecutionTransport::from_env` defaults to `in-process`; `as_str` selectors are stable. |
| `ledger_server_child`, `kernel_server_child` | Child entry points (no-op under a normal run). |

The process tests spawn both the ledger server and the kernel server either from
the real `agalma-ledger-server`/`agalma-kernel-server` binaries (when built, e.g.
full-workspace runs) or by re-exec'ing the test binary as the server
(`current_exe <child_test> --exact --nocapture`, socket/state-dir/ledger-socket
via `AGALMA_TEST_EP_*` env), so a package-scoped `cargo test -p agalma-conductor`
run still exercises the real process boundary. Both paths were run green (the
real-binary path and, with both binaries moved aside, the re-exec path).

`crates/agalma-kernel/tests/exec_api.rs` covers the adapter directly
(`start`/`status`/`cancel` mapping, `step`/`recover` gaps, `Send` seam,
`register_execution` handler completeness).

## Default transport decision

`ExecutionTransport::from_env()` reads `AGALMA_EXECUTION_TRANSPORT`
(`in-process` default, `process` optional). The **production default remains the
seed executor in-process**; nothing in `composition.rs`/`work.rs`/`lib.rs` is
switched to the kernel. `execution_process.rs` only exposes the supervised
kernel candidate so a caller/test can select and drive it. This matches the
bead's "Do NOT switch the production default".

## Deviations and design corrections (noted, not applied to `docs/b1-extraction.md`)

1. **`ExecutionOps` carries the kernel engine surface.** The frozen
   `ExecutionApi` cannot express effect dispatch/recovery without a handler
   registry, so the wire surface is `ExecutionApi` plus the kernel engine
   operations, unified by `ExecutionOps` (mirrors `LedgerOps`/`TaskQueueOps`). No
   contract schema changed.
2. **The engine surface is JSON for kernel-native payloads.** `agalma-ipc` is
   infrastructure and holds `ExecutionOps`; it must not depend on
   `agalma-kernel` (that edge would cycle once the kernel server depends on
   `agalma-ipc`). Kernel-native payloads (`Command`/`Inputs`/`ApplyOutcome`/
   `KernelState`/`TimerFire`/`Reclaim`) therefore cross as JSON `Value`s, and
   `KernelEngine<L>` implements `ExecutionOps` by encoding/decoding them. Contract
   types (`DispatchIntent`, `OperationReceipt`, `TimerRecord`, `StateSnapshot`)
   stay typed.
3. **`with_sandbox` now takes `Box<dyn SandboxApi + Send>` (and `sandbox_mut`
   returns `&mut (dyn SandboxApi + Send)`).** The server host requires the engine
   to be `Send` (`Handler: Send + Sync`); the non-`Send` boxed seam prevented
   that. The `FakeSandbox` and `SandboxClient` implementations are `Send`, so
   existing callers compile unchanged.
4. **Serde derives added to kernel state types** (`KernelState`, `OperationRecord`,
   `DirectiveRecord`, `ApplyOutcome`, `TimerFire`, `Reclaim`) so the engine
   surface can cross the boundary. No behavior change.
5. **Snapshot transfer by file (D3).** `export_snapshot_to`/`import_snapshot_from`
   write/read the canonical JSON snapshot at a caller-supplied path under the run
   state dir; only the path crosses the frame. The kernel's own
   `LedgerClient` continues to use the ledger boundary's snapshot file transfer
   internally.
6. **`ExecutionApi::start` does not begin a lease.** The kernel ties leases to
   explicit `begin_lease` (an `ExecutionOps` op), not to `Start`; the returned
   execution id still matches the seed's `ExecutionId::derive(task, generation)`.
7. **Kernel `Start` coarsens to `Pending`.** `Start` sets `ExecState::Ready`,
   which the coarse projection maps to `ExecutionState::Pending`; a `status`
   immediately after `start` reports `phase=Intake state=Pending` (the seed
   reports `Running`). This is the kernel's own coarse projection, not a
   boundary artifact.
8. **No single-writer lock on the kernel server.** The durable state owner is
   the ledger server (which keeps its own lock); the kernel server is stateless
   apart from its ledger connection and binds a unique socket per state dir.
9. **`Cargo.lock`.** Modified only for the two new intra-workspace dependency
   edges (`agalma-kernel -> agalma-ipc`, `agalma-conductor -> agalma-kernel`). No
   external crate was added.
10. **Unix socket path length.** Sockets live directly under the run state dir
    (`ledger.sock`, `execution.sock`) to stay within `SUN_LEN` (~104 bytes).
    Tests use short `target/test-runs/ep-<pid>-<n>/` run dirs.

## Consumer-invariance proof

No `crates/agalma-conductor/src/composition.rs`, `work.rs`, `phases.rs`,
`harness_binding.rs`, or `lib.rs` execution logic changed. `lib.rs` gains one
`pub mod execution_process;` declaration; `Cargo.toml` gains the
`agalma-kernel` dependency. No `crates/agalma-contracts/**` source changed; no
contract schema changed; no `docs/b1-extraction.md` edit. The architecture lint
is strict and green (`cargo test -p agalma-conductor --test architecture`:
`4 passed`): no implementation crate depends on or references another
implementation source (`agalma-ipc` is infrastructure and may be depended on).

`git diff --name-only` (tracked) plus new files, all within the allowed set:

```
Cargo.lock                                              # intra-workspace dep edges only
crates/agalma-conductor/Cargo.toml                      # + agalma-kernel dep (candidate wiring)
crates/agalma-conductor/src/lib.rs                      # + pub mod execution_process
crates/agalma-conductor/src/execution_process.rs        # new (candidate wiring; unreferenced by production paths)
crates/agalma-conductor/tests/execution_process.rs      # new
crates/agalma-ipc/src/lib.rs                            # module + re-exports
crates/agalma-ipc/src/execution_client.rs               # new
crates/agalma-ipc/src/execution_server.rs               # new
crates/agalma-kernel/Cargo.toml                          # + agalma-ipc dep
crates/agalma-kernel/src/engine.rs                       # serde derives; sandbox seam + Send
crates/agalma-kernel/src/exec_def.rs                     # serde derives on state records
crates/agalma-kernel/src/exec_api.rs                     # new (ExecutionApi/ExecutionOps adapter)
crates/agalma-kernel/src/lib.rs                          # exec_api + server modules
crates/agalma-kernel/src/ownership.rs                    # serde derives on Reclaim
crates/agalma-kernel/src/server.rs                       # new
crates/agalma-kernel/src/bin/agalma-kernel-server.rs     # new
crates/agalma-kernel/src/state.rs                        # serde derives on KernelState
crates/agalma-kernel/tests/exec_api.rs                   # new
docs/b1-execution-evidence.md                            # this file
```

## B1-F1 — handler-registry protocol (`agalma-5de`)

B1.7 left `ExecutionApi::step`/`recover` returning `UnsupportedCapability`
because the kernel separates the pure transition from handler-driven effect
dispatch and handlers are consumer-built Rust closures that cannot cross a
process boundary. B1-F1 defines the **handler-registry protocol**: the engine
(durable owner) *begins* an operation and hands the caller a serializable
`EffectContext`; the caller performs the effect through a caller-hosted
`EffectDispatch` registry; the engine *completes* the operation from a
serializable `EffectReport`. Only the nondeterministic effect half crosses the
boundary — exactly the split the kernel already draws between transition and
handler — while every durable step (duplicate/conflict/park/block checks, the
announce-before-effect commit, and the completion receipt) stays server-side.

The protocol is transport-agnostic: `KernelEngine<L>` implements the same three
operations in-process (`begin_step` / `begin_pending_step` / `complete_step`),
so an in-process binding and a process binding expose one surface.

### New interfaces

- `agalma_ipc::execution_handler`: `HandlerKey` (`name@version`), `EffectMode`
  (`Dispatch`/`Reconcile`), `EffectContext`, `EffectProbe`
  (`Present`/`Absent`/`Ambiguous`), `HandlerError`, `EffectReport`
  (`Present`/`Executed`/`Ambiguous`/`KnownFailure`/`UnknownOutcome`),
  `EffectDispatch` (caller-hosted registry), `StepBegin`
  (`Effect`/`Done`).
- `agalma_ipc::ExecutionOps` additions: `begin_step` (explicit
  `OperationRequest`), `begin_pending_step` (the durable pending operation),
  `complete_step`. Params `PendingStepCall`/`BeginStepCall`/`CompleteStepCall`.
  `EXECUTION_METHODS` grows 18 → 21.
- `agalma_ipc::ExecutionClient`: `with_effect_dispatch` / `has_effect_dispatch`.
  With a registry attached, `ExecutionApi::step`/`recover` drive the protocol;
  without one they forward to the server adapter and keep reporting
  `UnsupportedCapability` (the bare-engine gap).
- `agalma_kernel::activity`: `begin_activity` / `finish_activity` split
  `run_activity` into the durable begin (announce) and the commit (complete/park);
  `EffectMode`, `ActivityBegin`, `BeginOutcome`, `EffectReply`. `run_activity`
  itself is unchanged.

### Semantics

- `recover` mirrors `recover_pending`: reconcile each durable pending intent;
  `Present` completes it (reconciled), `Absent` leaves it pending, `Ambiguous`
  parks; a park/block halts further recovery for that execution.
- `step` drives the single pending operation of the execution; with no pending
  intent it projects `Idle`/`Parked` from `status`. `begin_step` additionally
  supports fresh dispatch: the intent is durable before the effect runs.
- Handler errors propagate faithfully (`KnownFailure`/`UnknownOutcome`), exactly
  as `run_activity` does.

### Files (B1-F1 delta, all in scope)

```
crates/agalma-ipc/src/execution_handler.rs    # new protocol types + EffectDispatch
crates/agalma-ipc/src/execution_client.rs     # registry field; step/recover; begin/complete ops
crates/agalma-ipc/src/execution_server.rs     # + begin_step/begin_pending_step/complete_step
crates/agalma-ipc/src/lib.rs                  # module + re-exports
crates/agalma-kernel/src/activity.rs          # begin_activity/finish_activity + EffectMode/EffectReply
crates/agalma-kernel/src/exec_api.rs          # ExecutionOps begin/complete + wire projections
crates/agalma-kernel/tests/exec_api.rs        # + 3 protocol tests
crates/agalma-conductor/tests/execution_process.rs  # + step parity + recover-after-rebind
docs/b1-execution-evidence.md                 # this section
```

### Results (B1-F1)

| Command | Observed |
|---|---|
| `cargo build --offline --workspace` | exit 0, clean |
| `cargo test --offline -p agalma-kernel -p agalma-execution` | exit 0, 0 failures (exec_api: 7 passed) |
| `cargo test --offline -p agalma-ipc` | exit 0, 48 + 14 passed |
| `cargo test --offline -p agalma-conductor --test execution_process --test architecture` | exit 0, execution_process 10 passed, architecture 4 passed |

- `handler_registry_step_parity_in_process_vs_process`: a durable pending
  operation reconciled in-process (`run_activity`) vs through the boundary
  (`ExecutionClient` + caller `EffectDispatch` → `begin_pending_step` /
  `complete_step`) produces the same receipt, zero pending work, and an identical
  normalized snapshot.
- `handler_registry_recover_after_rebind`: a pinned pending operation exported
  from binding A and imported into binding B is reconciled over the boundary by
  `recover()`, settling it on the new binding.
- Kernel `exec_api` additions: pending reconcile, fresh `begin_step` dispatch
  (announce durable before effect), duplicate-begin replay, and ambiguous park.

No contract schema changed; no production conductor path switched. `run_activity`
and the B1.7 adapters are behavior-preserving (all prior tests green).

## IPCX.7 — execution/kernel boundary on Connect (`agalma-ebg.8`)

Status: green. Design `docs/ipc-connect-design.md` (D1–D7, D7 boundary #6).
Pattern: `agalma-ebg.4` (sandbox, `dd18375`) / `agalma-ebg.7` (ledger,
`062a9b4`). The largest boundary now speaks the Connect transport; the frozen
`ExecutionApi` contract, the envelope semantics, the caller-side effect-dispatch
protocol (`agalma-5de`/B1-F1), and the snapshot-file transfer are unchanged.

### What changed

The typed `ExecutionApi`/`ExecutionOps` client and the kernel server moved onto
the per-boundary wire selector (`agalma_ipc::{Wire, WIRE_ENV}`, reused from
`agalma-ipc::connect`). The execution/kernel process wire defaults to `Connect`;
`AGALMA_IPC_WIRE=framed` restores framed for parity runs. The kernel server's own
`LedgerClient` connection uses the same wire, so the nested kernel→ledger edge
speaks the transport the supervisor selected for this boundary.

| Concern | Where |
|---|---|
| `ExecutionClient::connect_wire(socket, wire)` + `Backend::{Framed,Connect}` (framed `connect`/`from_client` retained) | `crates/agalma-ipc/src/execution_client.rs` |
| `serve_execution(server, socket, wire)` (`serve_wire` alias) | `crates/agalma-ipc/src/execution_server.rs` |
| `block_on_nested` — nested-safe `block_on` for a Connect client invoked from inside an async server handler | `crates/agalma-ipc/src/lib.rs` |
| `LedgerClient::call` uses `block_on_nested` (kernel server hosts it inside its Connect handler) | `crates/agalma-ipc/src/ledger_client.rs` |
| `serve` env-selects wire (Connect default); `serve_wire`; ledger/sandbox clients on the same wire | `crates/agalma-kernel/src/server.rs` |
| `execution_wire`, `ExecutionProcess::wire`, `spawn_with_spec_wire`, child `WIRE_ENV` export, matching client | `crates/agalma-conductor/src/execution_process.rs` |
| Wire parity + rebind evidence over both wires | `crates/agalma-conductor/tests/execution_process.rs` |
| Execution Connect conformance (new) | `crates/agalma-ipc/tests/execution_connect.rs` |

### New interfaces

- `agalma_ipc`: `ExecutionClient::connect_wire`; `execution_server::serve_execution`.
- `agalma_kernel::server`: `serve_wire`; `serve` env-selects wire (Connect
  default) and connects its ledger/sandbox clients over it.
- `agalma_conductor::execution_process`: `execution_wire`, `ExecutionProcess::wire`,
  `ExecutionProcess::spawn_with_spec_wire`.

### Nesting fix (scope note)

The kernel server is the first component that both *serves* over Connect and
talks to a peer over Connect: its `LedgerClient` is called synchronously from
inside the async Connect handler. The framed client is synchronous, so this was
latent; on Connect the client's blocking call (`Runtime::block_on`) would panic
("cannot start a runtime from within a runtime"). `block_on_nested` steps the
worker aside with `tokio::task::block_in_place` so the peer runtime's reactor
progresses. This is a one-file `ledger_client` change (the boundary the kernel
hosts, not a schema/behaviour change); the same latent hazard is fixed
defensively in the execution client. The other Connect clients are not nested
today and were left untouched.

### Results (IPCX.7)

| Command | Observed |
|---|---|
| `cargo build --offline --workspace` | exit 0, clean |
| `cargo test --offline -p agalma-ipc -p agalma-execution -p agalma-kernel` | exit 0, 0 failures (ipc 49 + 14 + 9 + 3 new + 4 + 3 + 2 + 2 + 2; execution 11; kernel 77) |
| `cargo test --offline -p agalma-conductor --test execution_process` | 10 passed, 0 failed |
| `cargo test --offline -p agalma-conductor --test architecture` | 6 passed, 0 failed |

New/changed evidence tests:

- `execution_api_over_connect_matches_framed` (new, `execution_connect`): a
  deterministic stub kernel over a `register_execution` server; the full typed
  surface — `start`/`status`/`cancel`, lease, `receipt`/`pending_dispatch`, and
  the caller-side handler-registry protocol (`begin_pending_step`/`complete_step`
  driving `step`, plus `recover`) — produces an identical transcript framed vs
  Connect.
- `execution_methods_include_handler_registry_ops`: `EXECUTION_METHODS` stays 21
  incl. `begin_step`/`begin_pending_step`/`complete_step`.
- `execution_connect_missing_socket_is_unavailable`.
- `shadow_parity_in_process_vs_process`: in-process vs framed vs Connect
  bindings produce an identical normalized engine-op transcript (lease → start →
  announce/dispatch → complete → timer → directive → cancel).
- `handler_registry_step_parity_in_process_vs_process`: the in-process
  `run_activity` reconcile outcome is reproduced over **both** wires (framed and
  Connect) by `ExecutionClient` + the caller `EffectDispatch` — same receipt,
  zero pending work, identical normalized snapshot.
- `handler_registry_recover_after_rebind`: a pinned pending operation exported
  from binding A and imported into binding B is reconciled by `recover()` over
  **both** wires.
- `execution_client_duplicate_id_replay_and_connection_loss` stays framed
  explicitly (raw `Client`/`UnixStream` conformance).
- `execution_process::tests::wire_defaults_to_connect_and_env_restores_framed`.

### Files (IPCX.7 delta)

```
crates/agalma-ipc/src/execution_client.rs     # connect_wire + Backend; nested-safe block_on
crates/agalma-ipc/src/execution_server.rs     # serve_execution
crates/agalma-ipc/src/ledger_client.rs        # nested-safe block_on
crates/agalma-ipc/src/lib.rs                  # block_on_nested + re-export
crates/agalma-ipc/tests/execution_connect.rs  # new
crates/agalma-kernel/src/server.rs            # serve_wire; serve env-selects wire
crates/agalma-conductor/src/execution_process.rs    # execution_wire + wire-aware spawn
crates/agalma-conductor/tests/execution_process.rs   # both-wire parity + rebind
docs/b1-execution-evidence.md                 # this section
```

No new dependency. `connectrpc`/`buffa` stay confined to `agalma-ipc`
(architecture test green). No `Cargo.toml`/`Cargo.lock` change; builds `--offline`.
`crates/agalma-contracts/**` unchanged; no contract schema changed. The
`agalma-5de` handler-registry resolution (caller-side effect dispatch) remains
faithful over both wires.
