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
