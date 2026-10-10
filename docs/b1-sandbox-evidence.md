# B1.3 — sandbox extraction evidence

Bead: `agalma-0ht.4`. Design: `docs/b1-extraction.md` (D1–D5, D7). Depends on
`agalma-0ht.3` (B1.2 harness extraction). HEAD at start: `dfa6142`.

Goal: move `SandboxApi` behind a process (Unix-socket) boundary without changing
any contract schema or consumer (phase) logic, and prove replacement through the
declared API with dual-binding parity, staged rebind, rollback, and IPC
conformance. As a derived requirement (D2), the already-extracted OpenCode
harness server switches from embedding `agalma-sandbox` in-process to driving the
extracted sandbox client.

## Deliverables

| # | Deliverable | Where |
|---|---|---|
| 1 | `SandboxClient` implementing `SandboxApi` over `Client` | `crates/agalma-ipc/src/sandbox_client.rs` |
| 2 | `register_sandbox` + `SANDBOX_METHODS` registration helper | `crates/agalma-ipc/src/sandbox_server.rs` |
| 2 | Sandbox server bin hosting `SeatbeltSandbox` + `server_command` | `crates/agalma-sandbox/src/bin/agalma-sandbox-server.rs` + `src/server.rs` |
| 3 | `SandboxProcess` supervisor + lazy `SandboxSource` (reuses `harness_process` spawn/stop) | `crates/agalma-conductor/src/sandbox_process.rs` |
| 3 | Build/verify harness construction through the transport factory | `crates/agalma-conductor/src/phases.rs`, `harness_process.rs` (`LiveHarness`, `open_opencode_harness`) |
| 4 | OpenCode harness server → sandbox client; `agalma-sandbox` dropped from normal deps; `SERVER_HOST_EDGES`/`is_server_host_edge` removed | `crates/agalma-harness-opencode/src/server.rs`, `Cargo.toml`; `crates/agalma-conductor/tests/architecture.rs` |
| 5 | `AGALMA_HARNESS_TRANSPORT` wired into the build/verify phase path | `phases.rs` (`run_live`/`run_verifier_session`/`run_reference`) |
| 6 | Parity / rebind / rollback / IPC-conformance tests | `crates/agalma-conductor/tests/sandbox_process.rs` |
| 7 | This evidence doc + invariance proof | `docs/b1-sandbox-evidence.md` |

### New interfaces

- `agalma_ipc`: `SandboxClient` + `map_ipc_error` (`sandbox_client.rs`);
  `register_sandbox`, `SANDBOX_METHODS` (`sandbox_server.rs`);
  `sandbox_server::ensure_all_handlers`.
- `agalma_sandbox`: `server` module — `SERVER_BIN`, `server_command`,
  `build_server`, `serve`, `run_from_args`, `server_info`, `default_socket_for`,
  `API_NAME`, `API_VERSION`, `IMPL_NAME`, `IMPL_VERSION`.
- `agalma_harness_opencode`: `SANDBOX_SOCKET_ENV`; `build_server` now takes
  `Box<dyn SandboxApi + Send>` (the client).
- `agalma_conductor::sandbox_process`: `SandboxProcess` (`spawn`,
  `spawn_with_spec`, `socket`, `pid`, `client`, `stop`), `SandboxSource`
  (`new`, `socket`, `ensure`, `client`, `pid`), `sandbox_server_spec`,
  `SANDBOX_SOCKET_NAME`.
- `agalma_conductor::harness_process`: `LiveHarness` (`InProcess`/`Process`,
  `attempt_pid`, `HarnessApi`), `OpenCodeHarnessInit`, `open_opencode_harness`;
  `spawn_supervised`/`stop_supervised` (shared supervision, `pub(crate)`).

## Results

All commands from the repo root, `--offline`.

| Command | Observed |
|---|---|
| `cargo test --offline -p agalma-ipc` | exit 0; `30 passed` (unit, incl. sandbox client) + `12 passed` (conformance) + doc-tests |
| `cargo test --offline -p agalma-sandbox` | exit 0; `17 passed` (unit) + `0 passed` (server bin) + `3 passed` (sandbox integration) + doc-tests |
| `cargo test --offline -p agalma-conductor` | exit 0; 17 test binaries `test result: ok`, 0 failures (incl. `sandbox_process`: `6 passed`, `architecture`: `4 passed`) |
| `cargo test --offline -p agalma-kernel` | exit 0; 13 test binaries `test result: ok`, 0 failures |
| `cargo test --offline` (full workspace) | exit 0; 61 test binaries `test result: ok`, 0 failures |

`tests/sandbox_process.rs` cases:

| Test | What it proves |
|---|---|
| `shadow_parity_in_process_vs_process_sandbox` | The same `LaunchSpec` scenario (`/bin/sh` writes a marker under the attempt dir, sleeps; `alive`; `terminate`) produces an identical, timing/ID-free transcript in-process (`SeatbeltSandbox` under a Tokio reactor) vs through the supervised sandbox server: `alive_before`, `file_written`, `process_group_gone`, non-empty termination detail. |
| `staged_rebind_keeps_inflight_child_alive` | An in-flight confined child launched on server A stays alive across a swap to candidate server B; the next operation launches on B; both children are distinct and terminable through their own server with process-group evidence. Mirrors the harness rebind intent (in-flight pinned; stateless components rebind between operations, D3). |
| `rollback_keeps_prior_child_and_server` | A candidate server is staged then rolled back (stopped, pid gone); the prior server and its already-launched child survive and remain terminable — no state loss. |
| `sandbox_client_duplicate_id_replay_and_connection_loss` | Raw `Client`: duplicate request id replays the recorded `launch` receipt (same `SandboxChild`, no second process); a fresh id launches a distinct child; both terminate; post-death call surfaces `Unavailable`/`UnknownOutcome`; a missing socket is `Unavailable`. |
| `sandbox_server_child` | Child entry point (no-op under a normal run). |
| `ready_marker_is_parsed` | The `READY {json}` handshake marker is recognized when libtest prefixes the line. |

The process tests spawn the sandbox server either from the real
`agalma-sandbox-server` binary (when built, e.g. full-workspace runs) or by
re-exec'ing the test binary as the server
(`current_exe sandbox_server_child --exact --nocapture`, socket via
`AGALMA_TEST_SANDBOX_SOCKET`), so a package-scoped `cargo test -p
agalma-conductor` run still exercises the real process boundary. Both paths were
run green.

## Default transport decision

`HarnessTransport::from_env()` reads `AGALMA_HARNESS_TRANSPORT`
(`in-process` default, `process` optional). The default remains **`in-process`**.

Deviation #2 from B1.2 is resolved: the build/verify construction sites now
consult the transport factory. `run_live`/`run_verifier_session` build the live
harness through `harness_process::open_opencode_harness(OpenCodeHarnessInit {
transport, .. })`, and `run_reference` builds the reference harness through
`open_reference_harness(kind, transport, ..)`. With the default the phase path is
byte-for-byte behaviourally unchanged; with `AGALMA_HARNESS_TRANSPORT=process`
the phase path spawns the selected component server (`ProcessBinding` /
`LiveHarness::Process`). Phase dispatch, ordering, recovery, and activity bodies
are unchanged; only the harness construction sites and the shared `drive`
helper's parameter type (`&mut LiveHarness` / `&mut dyn HarnessApi`) changed.

The OpenCode process branch is constructed but not exercised against a live
`opencode` install offline (it needs a real install and provider egress). The
reference transport is proven end-to-end offline by `tests/harness_process.rs`;
the sandbox transport is proven by `tests/sandbox_process.rs`.

## Conductor sandbox supervision and injection

`SandboxProcess` spawns `agalma-sandbox-server --socket <state_dir>/sandbox.sock`
as a process-group leader, waits for `READY`, and connects `SandboxClient`s. It
reuses the exact `spawn_supervised`/`stop_supervised` helpers extracted from
`harness_process` (one supervision implementation for every boundary).
`SandboxSource` starts the server lazily on first use and keeps it alive for the
phase activities.

Injection points (where a `Box<dyn SandboxApi>` was constructed or passed):

- `phases.rs` build (`run_live`) and verifier (`run_verifier_session`) harness
  construction: the in-process branch injects `Box::new(SandboxClient)` connected
  to the supervised server; the process branch passes the sandbox socket to the
  OpenCode server via `AGALMA_SANDBOX_SOCKET`.
- `agalma-harness-opencode/src/server.rs`: the server connects a `SandboxClient`
  from `AGALMA_SANDBOX_SOCKET` and injects it into `OpenCodeHarness::new`.

`composition.rs`/`work.rs` construct the phase activities (not a
`Box<dyn SandboxApi>`), so they are unchanged; `activities_with_mode` builds the
`SandboxSource` from `PhaseDeps::state_dir`.

## Component-server confinement (D2)

Per the B1 design clarification (commit `717a70f`), component servers are
**trusted code**: the conductor supervises them (spawn as group leader, `READY`
gate, `SIGTERM`/`SIGKILL` evidence) but does **not** OS-confine them. OS
confinement (`SandboxApi` + `PRODUCT_PROFILE`) is reserved for candidate or
untrusted implementations and the harness's confined `opencode serve` child,
which the sandbox server launches. The harness child's loopback allowance is
unchanged: provider egress tunnels through the parent-owned loopback proxy
(`provider_proxy`) and the S0b profile permits that loopback path. No new
profile work was required for component servers, because they are not confined.

## Deviations and design corrections (noted, not applied to `docs/b1-extraction.md`)

1. **Kernel seam is wiring-only; no code change.** `KernelEngine::with_sandbox`
   already takes `Box<dyn SandboxApi>`, and there is no production construction
   site in the conductor today (only kernel tests inject a fake). `SandboxClient`
   satisfies the seam; `agalma-ipc` asserts `SandboxClient: SandboxApi + Send`
   at compile time (`sandbox_client::tests::sandbox_client_is_a_send_sandbox_api`).
   No `agalma-kernel/**` file changed.
2. **Sandbox has no `Describe`/binding record.** `SandboxApi` is a stateless
   three-method contract, so the "staged rebind" is a supervised-server swap with
   the in-flight child pinned to its launching server, rather than a
   `BindingRecord` swap (the harness boundary, which has `Describe`, keeps the
   record-based rebind in `tests/harness_process.rs`).
3. **One-shot `SeatbeltSandbox::run` stays in-process.** `run` is an inherent
   one-shot used by the conductor's verify phase (exit code as verdict); it is
   **not** part of `SandboxApi`, so it is out of scope for this boundary.
   Likewise `build_probe` reconciles survivors through the in-process backend,
   because boot recovery probes run before any component server is supervised.
4. **Unix socket path length.** Sandbox and harness sockets live directly under
   the run state dir (`sandbox.sock`, `harness-<phase>-<attempt>.sock`) to stay
   within `SUN_LEN` (~104 bytes). Tests use short `target/test-runs/sp-<pid>-<n>/`
   run dirs.
5. **`Cargo.lock`.** Modified only for the new intra-workspace dependency edges
   (`agalma-sandbox -> agalma-ipc`; `agalma-sandbox` moved from
   `agalma-harness-opencode`'s normal deps to dev-deps). No external crate was
   added.

## Consumer-invariance proof

`crates/agalma-conductor/src/phases.rs` changes are limited to construction
sites: `run_live`/`run_verifier_session` build the harness through
`open_opencode_harness`, and `run_reference` builds it through
`open_reference_harness` (transport-aware). The shared `drive`/`drive_reference`
helpers take the harness behind the contract trait (`&mut LiveHarness` /
`&mut dyn HarnessApi`); phase dispatch, phase ordering, recovery, and every
activity body are unchanged. `build_probe` and the one-shot verify runs are
unchanged.

`git diff --name-only` (tracked) plus new files, all within the allowed set:

```
Cargo.lock                                                    # intra-workspace dep edges only
crates/agalma-conductor/src/harness_process.rs
crates/agalma-conductor/src/lib.rs
crates/agalma-conductor/src/phases.rs
crates/agalma-conductor/src/sandbox_process.rs                # new
crates/agalma-conductor/tests/architecture.rs
crates/agalma-conductor/tests/sandbox_process.rs              # new
crates/agalma-harness-opencode/Cargo.toml
crates/agalma-harness-opencode/src/lib.rs
crates/agalma-harness-opencode/src/server.rs
crates/agalma-ipc/src/lib.rs
crates/agalma-ipc/src/sandbox_client.rs                       # new
crates/agalma-ipc/src/sandbox_server.rs                       # new
crates/agalma-sandbox/Cargo.toml
crates/agalma-sandbox/src/bin/agalma-sandbox-server.rs        # new
crates/agalma-sandbox/src/lib.rs
crates/agalma-sandbox/src/server.rs                           # new
docs/b1-sandbox-evidence.md                                   # this file
```

No file under `crates/agalma-contracts/**` changed; no contract schema changed;
no file under `crates/agalma-kernel/**` changed. The B1.2 architecture carve-out
(`SERVER_HOST_EDGES` + `is_server_host_edge`) is removed, so both the Cargo.toml
edge check and the source-token check are strict again (verified by
`cargo test -p agalma-conductor --test architecture`: `4 passed`).
