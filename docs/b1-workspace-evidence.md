# B1.4 — workspace extraction evidence

Bead: `agalma-0ht.5`. Design: `docs/b1-extraction.md` (D1–D5, D7). Depends on
`agalma-0ht.4` (B1.3 sandbox extraction). HEAD at start: `127f49f`.

Goal: move `WorkspaceApi` (plus the M1 repo-mode operations the conductor drives)
behind a process (Unix-socket) boundary without changing any contract schema or
consumer (phase) logic, and prove replacement through the declared API with
dual-binding parity, staged rebind, rollback, and IPC conformance.

## Deliverables

| # | Deliverable | Where |
|---|---|---|
| 1 | `WorkspaceClient` implementing `WorkspaceApi` + `WorkspaceOps` over `Client` | `crates/agalma-ipc/src/workspace_client.rs` |
| 1 | `WorkspaceOps` trait (frozen `WorkspaceApi` + M1 repo-mode surface) | `crates/agalma-ipc/src/workspace_client.rs` |
| 2 | `register_workspace` + `WORKSPACE_METHODS` registration helper | `crates/agalma-ipc/src/workspace_server.rs` |
| 2 | Workspace server bin hosting `Workspace` + `server_command` | `crates/agalma-workspace/src/bin/agalma-workspace-server.rs` + `src/server.rs` |
| 3 | `WorkspaceProcess` supervisor + `WorkspaceHandle` transport handle (reuses `spawn_supervised`/`stop_supervised`) | `crates/agalma-conductor/src/workspace_process.rs` |
| 3 | Construction sites switched to the transport handle | `crates/agalma-conductor/src/composition.rs`, `work.rs`, `phases.rs` |
| 4 | Parity / rebind / rollback / IPC-conformance tests | `crates/agalma-conductor/tests/workspace_process.rs` |
| 5 | This evidence doc + invariance proof | `docs/b1-workspace-evidence.md` |

### New interfaces

- `agalma_ipc`: `WorkspaceClient` (`connect`, `from_client`), `map_ipc_error`,
  param structs (`PrepareParams`, `PrepareRepoParams`, `CheckoutCall`,
  `IntegrateParams`, `IntegrateOriginParams`, `TagParams`, `RevertParams`,
  `PrepareVerifyParams`, `OriginReceiptParams`, `RevertOriginParams`),
  `WorkspaceOps` (the server-hosted full surface);
  `register_workspace`, `WORKSPACE_METHODS`,
  `workspace_server::ensure_all_handlers`.
- `agalma_workspace`: `impl WorkspaceOps for Workspace` (`src/lib.rs`);
  `server` module — `SERVER_BIN`, `server_command`, `build_server`, `serve`,
  `run_from_args`, `server_info`, `default_socket_for`, `API_NAME`,
  `API_VERSION`, `IMPL_NAME`, `IMPL_VERSION`.
- `agalma_conductor::workspace_process`: `WorkspaceTransport` (`from_env`,
  `as_str`), `TRANSPORT_ENV`, `WORKSPACE_SOCKET_NAME`, `workspace_server_spec`,
  `WorkspaceProcess` (`spawn`, `spawn_with_spec`, `socket`, `pid`, `client`,
  `stop`), `WorkspaceHandle` (`in_process`, `process`, `from_transport`).

### Why `WorkspaceOps`, not just `WorkspaceApi`

The conductor drives more than the frozen six-method `WorkspaceApi`: the verify
and integrate phases call the M1 repo-mode operations
(`prepare_repo`/`commit_candidate`/`prepare_verify_checkout`/`origin_receipt`/
`integrate_origin`/`integrated_receipt`/`revert_origin`/`candidate_sha`). To keep
phase/work logic unchanged (D5) while carrying those operations across the
boundary, `agalma-ipc` defines `WorkspaceOps: WorkspaceApi` holding the full
surface. `agalma-workspace` implements it for the git-backed `Workspace`;
`WorkspaceClient` and the conductor's `WorkspaceHandle` implement it over the
transport (the handle delegates to whichever backend is bound). This is an
internal helper trait in infrastructure; no contract schema changed.

## Results

All commands from the repo root, `--offline`.

| Command | Observed |
|---|---|
| `cargo test --offline -p agalma-ipc` | exit 0; `34 passed` (unit, incl. workspace client) + `12 passed` (conformance) + doc-tests |
| `cargo test --offline -p agalma-workspace` | exit 0; `3 passed` (server module) + `0 passed` (server bin) + `11 passed` (workspace integration) + doc-tests |
| `cargo test --offline -p agalma-conductor` | exit 0; 17 test binaries `test result: ok`, 0 failures (incl. `workspace_process`: `6 passed`, `architecture`: `4 passed`) |
| `cargo test --offline` (full workspace) | exit 0; 0 failures |

`tests/workspace_process.rs` cases:

| Test | What it proves |
|---|---|
| `shadow_parity_template_ops` | The template-mode scenario (`prepare`/`diff`/`integrate`/`tag`/`revert`) produces an identical, sha/timing-free transcript in-process (`Workspace`) vs through the supervised workspace server: the diff carries the candidate edit, the CAS is a fast-forward (`candidate_sha == result_sha`), the tag points at the candidate, and `revert` restores the base tree. The produced `main^{tree}` digests match. |
| `shadow_parity_repo_ops` | The repo-mode scenario the checkout/verify/integrate phases use (`prepare_repo`/`diff`/`commit_candidate`/`prepare_verify_checkout`/`origin_receipt`/`integrate_origin`/`revert_origin`) produces an identical transcript in-process vs via the server, including the `main^{tree}` digest and the "no receipt before / candidate receipt after" integration observations. |
| `staged_rebind_keeps_pinned_checkout_usable` | An in-flight checkout (prepared + committed on server A) stays pinned across a swap: candidate server B integrates A's committed SHA into the origin, the origin `main` lands on that SHA, and A remains alive and reports the same candidate. Mirrors the stateless-component rebind intent (D3: rebind between operations; the artifact is pinned to its preparing server). |
| `rollback_keeps_prior_checkout_and_server` | A candidate server is staged then rolled back (stopped, pid gone); the prior server and its committed checkout survive and still land the candidate into the origin — no state loss. |
| `workspace_client_duplicate_id_replay_and_connection_loss` | Raw `Client`: a duplicate request id replays the recorded `prepare_repo` receipt; a duplicate `integrate_origin` id replays the receipt while a fresh id re-runs and returns `Conflict` (main moved) — proving the effect ran once; post-death call surfaces `Unavailable`/`UnknownOutcome`; a missing socket is `Unavailable`. |
| `workspace_server_child` | Child entry point (no-op under a normal run). |

The process tests spawn the workspace server either from the real
`agalma-workspace-server` binary (when built, e.g. full-workspace runs) or by
re-exec'ing the test binary as the server
(`current_exe workspace_server_child --exact --nocapture`, socket via
`AGALMA_TEST_WORKSPACE_SOCKET`, base dir via `AGALMA_TEST_WORKSPACE_BASE_DIR`),
so a package-scoped `cargo test -p agalma-conductor` run still exercises the real
process boundary. Both paths were run green (the real-binary path and, with the
binary moved aside, the re-exec path).

## Default transport decision

`WorkspaceTransport::from_env()` reads `AGALMA_WORKSPACE_TRANSPORT`
(`in-process` default, `process` optional). The default remains **`in-process`**,
matching B1.2/B1.3. The construction sites now consult the transport factory:
`Composition::build` and `work::build_executor` build the workspace through
`WorkspaceHandle::from_transport(&state_dir, WorkspaceTransport::from_env())`.
With the default the phase path is behaviourally unchanged; with
`AGALMA_WORKSPACE_TRANSPORT=process` the sites spawn the supervised workspace
server (rooted at the run state dir, socket `<state_dir>/workspace.sock`) and
bind a `WorkspaceClient`.

## Conductor workspace supervision

`WorkspaceProcess` spawns
`agalma-workspace-server --socket <state_dir>/workspace.sock --base-dir <state_dir>`
as a process-group leader, waits for `READY`, and connects `WorkspaceClient`s. It
reuses the exact `spawn_supervised`/`stop_supervised` helpers shared with the
harness/sandbox boundaries (one supervision implementation for every boundary).
The server is rooted at the same base dir the in-process backend used, so
`<base_dir>/runs/<exec>` checkout paths are identical across bindings.

`WorkspaceHandle` holds either the in-process `Workspace` or a `WorkspaceClient`
plus the `Rc<WorkspaceProcess>` that keeps the server alive for the handle's
lifetime (stopped on drop). It exposes the full surface by delegation, so the
phase/work call bodies are unchanged — only the constructed type differs.

## Deviations and design corrections (noted, not applied to `docs/b1-extraction.md`)

1. **`WorkspaceOps` carries the M1 repo-mode surface.** The design names
   `WorkspaceClient` as implementing "the same contract trait"; the conductor
   also drives the M1 repo-mode operations, so the wire surface is the frozen
   `WorkspaceApi` plus those operations, unified by `WorkspaceOps`. No contract
   schema changed.
2. **Workspace has no `Describe`/binding record.** `WorkspaceApi` has no
   `Describe`, so (as for the sandbox boundary) the "staged rebind" is a
   supervised-server swap with the in-flight checkout pinned to its preparing
   server, rather than a `BindingRecord` swap (the harness boundary keeps the
   record-based rebind).
3. **The workspace server needs a base dir.** Unlike the sandbox/harness servers
   (socket only), the git backend is rooted at a directory, so the server takes
   `--base-dir <path>`; `run_from_args` parses both `--socket` and `--base-dir`.
4. **No Tokio runtime in the workspace server.** The backend shells out to `git`
   via `std::process`, so the serial serve loop runs synchronously (mirroring the
   reference harness server, not the Tokio-based sandbox/OpenCode servers).
5. **Unix socket path length.** The workspace socket lives directly under the run
   state dir (`workspace.sock`) to stay within `SUN_LEN` (~104 bytes). Tests use
   short `target/test-runs/wp-<pid>-<n>/` run dirs.
6. **`Cargo.lock`.** Modified only for the new intra-workspace dependency edge
   (`agalma-workspace -> agalma-ipc`). No external crate was added.

## Consumer-invariance proof

`crates/agalma-conductor/src/phases.rs` changes are limited to the import and the
field/parameter type (`Rc<RefCell<Workspace>>` → `Rc<RefCell<WorkspaceHandle>>`);
every call site, phase dispatch, ordering, recovery, and activity body is
unchanged. `composition.rs`/`work.rs` changes are limited to the workspace
construction site (now `WorkspaceHandle::from_transport`). `reconcile.rs` is
unchanged (it never used the workspace). The conductor tests that construct a
workspace for the activities were updated to construct the handle
(`WorkspaceHandle::in_process`).

`git diff --name-only` (tracked) plus new files, all within the allowed set:

```
Cargo.lock                                                    # intra-workspace dep edge only
crates/agalma-conductor/src/composition.rs                    # construction site
crates/agalma-conductor/src/lib.rs                            # module registration
crates/agalma-conductor/src/phases.rs                         # field/param type only
crates/agalma-conductor/src/work.rs                           # construction site
crates/agalma-conductor/src/workspace_process.rs              # new
crates/agalma-conductor/tests/conductor.rs                    # handle construction
crates/agalma-conductor/tests/fencing.rs                      # handle construction
crates/agalma-conductor/tests/rebind.rs                       # handle construction
crates/agalma-conductor/tests/recovery.rs                     # handle construction
crates/agalma-conductor/tests/workspace_process.rs            # new
crates/agalma-conductor/tests/xcrun.rs                        # handle construction
crates/agalma-ipc/src/lib.rs                                  # module + re-exports
crates/agalma-ipc/src/workspace_client.rs                     # new
crates/agalma-ipc/src/workspace_server.rs                     # new
crates/agalma-workspace/Cargo.toml                            # + agalma-ipc dep
crates/agalma-workspace/src/lib.rs                            # WorkspaceOps impl + server module
crates/agalma-workspace/src/server.rs                         # new
crates/agalma-workspace/src/bin/agalma-workspace-server.rs    # new
docs/b1-workspace-evidence.md                                 # this file
```

No file under `crates/agalma-contracts/**` changed; no contract schema changed;
no `docs/b1-extraction.md` edit. The architecture lint is strict and green
(`cargo test -p agalma-conductor --test architecture`: `4 passed`): no
implementation crate depends on another, and no implementation source references
another implementation crate.

# IPCX.4 — workspace boundary on the Connect transport

Bead: `agalma-ebg.5`. Design: `docs/ipc-connect-design.md` (D1–D7). Foundation:
`agalma-ebg.2` (`ipc.proto`, Connect stack + pins, UDS accept loop, client core,
Connect-level conformance harness). Depends on `agalma-ebg.4` (IPCX.3 sandbox
boundary, the migration pattern). HEAD at start: `4628801`.

Goal: move the already-extracted `WorkspaceApi` (+ the `WorkspaceOps` repo-mode
surface) process boundary onto the Connect transport (IPCX D7, boundary #3)
**without changing the envelope semantics or any consumer logic**, and prove it
with the B1 recipe — dual-binding parity, staged rebind, rollback, IPC
conformance — extended to the Connect wire. As in IPCX.2/IPCX.3, the
per-boundary evidence named by D4 (`docs/ipcx-<boundary>-evidence.md`) is folded
into this file so the workspace boundary's B1 and IPCX evidence stay together.

## What migrated

The typed workspace client and its method-registration helper stay the entry
points; only the byte transport under them changed.

| Concern | Where |
|---|---|
| `WorkspaceClient` speaks either wire (`connect_wire`); `connect` stays framed | `crates/agalma-ipc/src/workspace_client.rs` |
| `serve_workspace(server, socket, wire)` workspace serve entry point | `crates/agalma-ipc/src/workspace_server.rs` |
| Workspace server serves the selected wire (`serve` reads `AGALMA_IPC_WIRE`) | `crates/agalma-workspace/src/server.rs` |
| `WorkspaceProcess` selects the wire, exports it to the child, connects matching client | `crates/agalma-conductor/src/workspace_process.rs` |

- The workspace process wire **defaults to `Connect`** (`workspace_wire()`); a
  parity run sets `AGALMA_IPC_WIRE=framed` to compare the two against the same
  scenario. The selected wire is exported to the child in the same variable, so
  the server serves the matching transport. Both wires announce the identical
  `READY {json}` line, so the supervisor handshake is unchanged; `WorkspaceApi`
  is `Describe`-free, so there is no binding gate to keep wire-agnostic.
- Envelope semantics are preserved verbatim on both wires: `v`/`id`/`method`/
  `params_json`; duplicate-id receipt replay; `UnknownOutcome`/contract errors
  in-band; version refusal; unknown-method `UnsupportedCapability`. The Connect
  path reuses `Server::dispatch`, so no envelope code was forked.
- `WorkspaceOps` (the frozen `WorkspaceApi` plus the M1 repo-mode operations) is
  unchanged; only the transport beneath it differs.
- No new dependency. `connectrpc`/`buffa` stay confined to `agalma-ipc`
  (architecture test green); no `Cargo.toml`/`Cargo.lock` change.

### New interfaces

- `agalma_ipc`: `workspace_client::WorkspaceClient::connect_wire` (framed
  `connect`/`from_client` retained); `workspace_server::serve_workspace`.
- `agalma_workspace::server`: `serve_wire`; `serve` now reads `AGALMA_IPC_WIRE`
  (`connect` default, `framed` optional).
- `agalma_conductor::workspace_process`: `workspace_wire`,
  `WorkspaceProcess::wire`, `WorkspaceProcess::spawn_with_spec_wire(.., wire, ..)`
  (`spawn`/`spawn_with_spec` retained, defaulting to `workspace_wire`).

## Results

All commands from the repo root, `--offline`.

| Command | Observed |
|---|---|
| `cargo build --offline --workspace` | exit 0 |
| `cargo test --offline -p agalma-ipc -p agalma-workspace` | ipc: 48 unit + 14 conformance + 9 connect-conformance + 4 harness-connect + 2 sandbox-connect + 2 taskqueue-connect + 2 ledger-connect + **2 workspace-connect** (new) + doc-tests; workspace: 3 unit + 11 integration; 0 failures |
| `cargo test --offline -p agalma-conductor --test workspace_process` | **7 passed; 0 failed** |
| `cargo test --offline -p agalma-conductor --lib` | 143 passed; 0 failed |
| `cargo test --offline -p agalma-conductor --test architecture` | 6 passed; 0 failed |

`tests/workspace_connect.rs` (new, `agalma-ipc`):

| Test | What it proves |
|---|---|
| `workspace_api_over_connect_matches_framed` | The typed `WorkspaceClient` drives the full `WorkspaceOps` surface (all six `WorkspaceApi` methods plus the eight repo-mode operations) over a `register_workspace` server; a framed and a Connect server produce an **identical typed transcript**. A deterministic stub workspace keeps the transcript clock/disk-free. |
| `workspace_connect_missing_socket_is_unavailable` | A missing Connect socket surfaces as `Unavailable`. |

`tests/workspace_process.rs` (conductor):

| Test | What it proves |
|---|---|
| `wire_parity_framed_vs_connect_matches_in_process` (new) | The same template-mode scenario yields an identical normalized transcript in-process, over the **framed** process wire, and over the **Connect** process wire. This is the dual-binding parity proof for the migrated boundary. |
| `shadow_parity_template_ops` | In-process vs process parity (`prepare`/`diff`/`integrate`/`tag`/`revert`), now over **Connect** (`process.client()` over the stored wire). |
| `shadow_parity_repo_ops` | Repo-mode parity (`prepare_repo`/`commit_candidate`/`prepare_verify_checkout`/`origin_receipt`/`integrate_origin`/`revert_origin`), now over **Connect**. |
| `staged_rebind_keeps_pinned_checkout_usable` | An in-flight checkout (prepared + committed on server A) stays pinned across a swap: candidate server B integrates A's committed SHA, and A stays alive and reports the same candidate — over **Connect**. |
| `rollback_keeps_prior_checkout_and_server` | Candidate retired (pid gone); prior server and its committed checkout survive and still land the candidate — no state loss. |
| `workspace_client_duplicate_id_replay_and_connection_loss` | Raw framed `Client`: duplicate-id `prepare_repo` receipt replay, duplicate `integrate_origin` id replays while a fresh id re-runs and returns `Conflict`, post-death `Unavailable`/`UnknownOutcome`, missing socket `Unavailable` (framed path still proven; the server is spawned explicitly framed). |
| `workspace_server_child` | Child entry point serving the env-selected wire (no-op under a normal run). |

## Consumer-invariance proof

Phase/work logic is unchanged: `WorkspaceHandle` keeps its surface
(`in_process`/`process`/`from_transport`) and delegates to whichever backend is
bound; only the private wire selection and the client-construction sites changed.
No contract schema changed. `git diff --name-only` (tracked) plus the new file,
all within the allowed set:

```
crates/agalma-conductor/src/workspace_process.rs
crates/agalma-conductor/tests/workspace_process.rs
crates/agalma-ipc/src/workspace_client.rs
crates/agalma-ipc/src/workspace_server.rs
crates/agalma-ipc/tests/workspace_connect.rs            # new
crates/agalma-workspace/src/server.rs
docs/b1-workspace-evidence.md                           # this file
```

No file under `crates/agalma-contracts/**` changed; no contract schema changed.
`crates/agalma-ipc/src/lib.rs` is untouched (the new entry points are reached
through their module paths). The harness/sandbox/taskqueue/ledger boundary files
are untouched.

## Deviations and notes

1. The IPCX design D4 names a per-boundary `docs/ipcx-<boundary>-evidence.md`;
   this bead's scope extends the existing B1 workspace evidence instead, keeping
   the framed and Connect proofs side by side (matching IPCX.2/IPCX.3).
2. The workspace process wire defaults to `Connect`. `WorkspaceApi` is
   `Describe`-free, so the "staged rebind" remains a supervised-server swap with
   the in-flight checkout pinned to its preparing server (B1.4 note 2), now over
   the Connect wire.
3. `WorkspaceProcess::spawn`/`spawn_with_spec` keep their signatures (defaulting
   to `workspace_wire()`); `spawn_with_spec_wire` is the explicit-wire entry
   point the parity test uses. The re-exec'd child serves the env-selected wire
   because `spawn_with_spec_wire` exports `AGALMA_IPC_WIRE` to it.
4. `serve_wire` installs a multithread Tokio runtime for both wires. The git
   backend still shells out through `std::process` and needs no reactor itself,
   but the Connect accept loop does, so (unlike B1.4) the server now hosts one.
5. The Connect accept loop logs peer resets (`connect connection error`) at most;
   the workspace scenario's heavier disconnect/stopping sequence surfaces these
   benign lines in test output without affecting the transcripts.

