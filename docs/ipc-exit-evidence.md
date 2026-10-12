# IPCX exit evidence — ConnectRPC transport for nano services

Bead: `agalma-ebg.9` (IPCX.8). Epic: `agalma-ebg` (IPC v2). Date: 2026-10-12.
Design: `docs/ipc-connect-design.md` (`agalma-ebg.1`, commit `ca172d2`). B1
recipe: `docs/b1-extraction.md` (its D1 superseded for transport only).
Foundation: `agalma-ebg.2` (commit `79e3b42`). HEAD at exit: `ca64d9e`.

Epic exit criteria (`agalma-ebg` description): swap the cross-process
nano-service transport from custom framed JSON to ConnectRPC over Unix domain
sockets, envelope-first — `agalma-contracts` serde JSON schemas and the ledger's
persisted JSON stay unchanged; all six Tier-1 seams migrate with the B1
evidence recipe (dual-binding parity, staged rebind, rollback); the
`agalma-www` head-of-line issue is resolved; no consumer-code diff beyond
construction sites.

## Component map — every Tier-1 seam on the Connect envelope

The byte transport under every already-extracted process boundary is now
ConnectRPC (Connect protocol, HTTP/2 over a Unix domain socket) with the frozen
`IpcRequest`/`IpcResponse`/`IpcError` envelope
(`crates/agalma-ipc/proto/ipc.proto`). Envelope semantics are unchanged: `v`,
`id`, `method`, `params_json`; duplicate-`id` receipt replay; in-band
`ContractError` codes; `UnknownOutcome`; version refusal; unknown-method
`UnsupportedCapability`. The in-process bindings are untouched.

Each boundary reads the wire it serves from the shared selector
`Wire::from_env(Wire::Connect)` — the env var is `AGALMA_IPC_WIRE`
(`connect` default, `framed` restores the B1 length-prefixed transport for
parity runs). This is distinct from the B1 binding selector
`AGALMA_*_TRANSPORT` (`in-process` default, `process` optional), which chooses
in-process vs process, not the wire under a process binding.

| Boundary | Bead | Commit | Wire default | Wire selector | Evidence | Parity | Rebind | Rollback |
|---|---|---|---|---|---|---|---|---|
| `HarnessApi` | `agalma-ebg.3` | `1c1d89c` | `Connect` | `AGALMA_IPC_WIRE` (`Wire::from_env(Wire::Connect)`) | `docs/b1-harness-evidence.md` §IPCX.2 | in-process vs framed vs Connect, typed transcript + wire-independent bind plan | pinned in-flight op across swap | prior binding restored, no state loss |
| `SandboxApi` | `agalma-ebg.4` | `dd18375` | `Connect` | `AGALMA_IPC_WIRE` | `docs/b1-sandbox-evidence.md` §IPCX.3 | in-process vs framed vs Connect, launch/alive/terminate | between operations (supervised swap) | prior server, no state loss |
| `WorkspaceApi` | `agalma-ebg.5` | `2adf23c` | `Connect` | `AGALMA_IPC_WIRE` | `docs/b1-workspace-evidence.md` §IPCX.4 | full `WorkspaceOps` surface, framed vs Connect | pinned checkout across swap | prior checkout + server survive |
| `TaskQueueApi` | `agalma-ebg.6` | `5a0a359` | `Connect` | `AGALMA_IPC_WIRE` | `docs/b1-taskqueue-evidence.md` §IPCX.5 | scratch bd sequences, projection compare | pinned claim across swap | prior claim + server survive |
| `LedgerApi` | `agalma-ebg.7` | `062a9b4` | `Connect` | `AGALMA_IPC_WIRE` | `docs/b1-ledger-evidence.md` §IPCX.6 | 33 methods incl. control receipts; file-based snapshot path unchanged | quiesced export/import, pinned op settles | prior state + server survive |
| `ExecutionApi` / kernel | `agalma-ebg.8` | `ca64d9e` | `Connect` | `AGALMA_IPC_WIRE` | `docs/b1-execution-evidence.md` §IPCX.7 | engine-op transcript + caller-side effect dispatch (`step`/`recover`) | export/import rebind, `recover()` reconciles pending | prior binding survives with pending work |

Sandbox has no `in-process` transport (it is always the supervised process
boundary, per B1); every other boundary keeps its B1 `AGALMA_*_TRANSPORT`
binding selector and only gains the `AGALMA_IPC_WIRE` wire selector.

Foundation (`agalma-ebg.2`, `79e3b42`): `ipc.proto` envelope, `connectrpc-build`
codegen in `build.rs`, the UDS accept loop and Connect client core
(`crates/agalma-ipc/src/connect.rs`), and the Connect-level conformance harness
(`crates/agalma-ipc/tests/connect_conformance.rs`). `agalma-www` (IPCX.1) is
closed by the HTTP/2 multiplexing this replaces serial framed connections with.

## Per-boundary evidence links

Each IPCX section extends the boundary's existing B1 evidence doc so the framed
and Connect proofs sit side by side (IPCX.2 deviation 1):

- Harness — `docs/b1-harness-evidence.md`, section
  `# IPCX.2 — harness boundary on the Connect transport`.
- Sandbox — `docs/b1-sandbox-evidence.md`, section
  `# IPCX.3 — sandbox boundary on the Connect transport`.
- Workspace — `docs/b1-workspace-evidence.md`, section
  `# IPCX.4 — workspace boundary on the Connect transport`.
- Task queue — `docs/b1-taskqueue-evidence.md`, section
  `# IPCX.5 — task-queue boundary migration to Connect (agalma-ebg.6)`.
- Ledger — `docs/b1-ledger-evidence.md`, section
  `## IPCX.6 — ledger boundary migration to Connect (agalma-ebg.7)`.
- Execution / kernel — `docs/b1-execution-evidence.md`, section
  `## IPCX.7 — execution/kernel boundary on Connect (agalma-ebg.8)`.

Per-bead implementation reports: `target/worker-output/agalma-ebg.2.md` …
`target/worker-output/agalma-ebg.8.md`.

## Architecture tests (design D6)

`crates/agalma-conductor/tests/architecture.rs` — 9 tests, all green
(`cargo test --offline -p agalma-conductor --test architecture`). The design D6
checks and the IPCX.8 additions:

| Test | What it pins |
|---|---|
| `implementation_crates_do_not_depend_on_each_other_in_cargo_toml` | no impl→impl normal dependency edge (B1.1) |
| `implementation_crates_do_not_reference_each_other` | no cross-impl source reference (B1.1) |
| `connect_stack_is_confined_to_agalma_ipc` | `connectrpc`/`buffa` **and** the build-time `connectrpc-build`/`buffa-codegen` appear in normal *or* build dependency tables only in `agalma-ipc`; `agalma-ipc` owns all four (IPCX.1 + IPCX.8) |
| `connect_stack_source_is_confined_to_agalma_ipc` | no crate but `agalma-ipc` *names* the Connect stack in source (comments/strings stripped) — the transport is reached only through the `agalma-ipc` boundary (IPCX.8) |
| `implementation_crates_do_not_reference_conductor` | no impl crate imports `agalma_conductor` in source (IPCX.1) |
| `implementation_crates_do_not_depend_on_conductor_in_cargo_toml` | no impl crate declares a normal/build/dev Cargo dependency on `agalma-conductor` (IPCX.8) |
| `build_dependency_parser_reads_only_build_table` | parser unit test for the build-dependency scan (IPCX.8) |
| `cargo_toml_scan_ignores_dev_dependencies` | parser unit test: dev-deps out of scope for impl→impl (B1.1) |
| `comment_stripper_ignores_comments_but_keeps_code` | scanner unit test (B1.1) |

The confinement proofs are manifest-level (normal + build tables) and
source-level; dependency direction `conductor -> implementations -> contracts`
holds at both levels.

## Offline build proof

The Connect stack was fetched once at the foundation
(`agalma-ebg.2`: `cargo fetch`), after which every bead built `--offline` with
no hidden fetch (design D2; `docs/m0-dependencies.md`).

| Command | Result |
|---|---|
| `cargo fetch --offline` | exit 0, **empty output** — the fetch-needed list is empty (all reads already cached) |
| `cargo build --offline --workspace` | exit 0, clean (compiles all crates incl. the Connect stack) |
| `cargo test --offline -p agalma-conductor --test architecture` | 9 passed, 0 failed |

No new dependency was added by any boundary bead (IPCX.2–IPCX.7); the pins
(`connectrpc = 0.9.1`, `buffa = 0.9.2`, `connectrpc-build = 0.9.0`,
`buffa-codegen = 0.9.2`, plus the hyper/hyper-util/tower/http transitives) are
unchanged from the foundation.

## Conformance-suite summary

`cargo test --offline -p agalma-ipc` — **88 passed, 0 failed** (49 unit + 39
integration; doc-tests ignored):

| Suite | Tests | Focus |
|---|---|---|
| `src/lib.rs` unittests | 49 | envelope, framing, receipts, dispatch, errors |
| `tests/conformance.rs` | 14 | framed-wire conformance (envelope semantics retained) |
| `tests/connect_conformance.rs` | 9 | Connect-level: echo round trip, duplicate-id replay, replay across reconnect, `UnknownOutcome` in-band, contract error, unknown method, missing socket → `Unavailable`, version mismatch → `Protocol`, HTTP/2 multiplexing |
| `tests/harness_connect.rs` | 4 | `HarnessApi` framed-vs-Connect transcript, live `describe`, missing socket, wire selector |
| `tests/sandbox_connect.rs` | 2 | `SandboxApi` framed-vs-Connect, missing socket |
| `tests/workspace_connect.rs` | 2 | `WorkspaceOps` framed-vs-Connect, missing socket |
| `tests/taskqueue_connect.rs` | 2 | task-queue framed-vs-Connect, missing socket |
| `tests/ledger_connect.rs` | 3 | 33-method surface framed-vs-Connect, missing socket, control-receipt ops present |
| `tests/execution_connect.rs` | 3 | execution + handler-registry framed-vs-Connect, missing socket, handler ops present |

Per-boundary conductor process suites run the same scenario over both wires and
match the in-process transcript (counts recorded at each bead landing):
`harness_process` 14, `sandbox_process` 7, `workspace_process` 7,
`taskqueue_process` 8, `ledger_process` 10, `execution_process` 10.

## Consumer invariance

- `crates/agalma-contracts/**` is unchanged across the epic: no schema, ID,
  trait, or error change (each boundary evidence doc carries its
  `git diff --name-only` proof).
- Conductor changes are construction-site/wiring only: private wire selection,
  `spawn_with_spec_wire`s, client construction. Phase/activity logic is
  unchanged; the process transport remains exercised only by boundary tests, so
  the Connect wire default changes no production path.
- Envelope semantics are preserved verbatim — the Connect path reuses
  `Server::dispatch`; no envelope code was forked.

## Residual risks and deviations

1. **Hand-rolled proto reader (foundation deviation, tracked in
   `docs/m0-dependencies.md`).** `connectrpc-build` shells out to `protoc`/`buf`;
   neither exists offline and neither may be downloaded, so
   `crates/agalma-ipc/build.rs` reads `ipc.proto` with a small in-repo proto3
   reader and hands the codegen a `FileDescriptorSet`. `ipc.proto` stays the
   source of truth; codegen runs at build time; builds stay offline.
2. **`Stream` RPC reserved, not generated.** The envelope proto carries only
   `rpc Call(...)`; server streaming for future event push is reserved in a
   comment. No Tier-1 seam needs streaming for parity. The hand-rolled proto
   reader does not yet parse `stream` modifiers, so adding the reserved RPC
   later needs a reader extension.
3. **hyper-util accept loop.** `connectrpc` 0.9.1 has no public
   `Server::serve_connection`; the UDS accept loop uses
   `hyper_util::server::conn::auto::Builder::serve_connection`, one task per
   accepted connection (`crates/agalma-ipc/src/connect.rs::serve`). This is the
   documented UDS path and supplies the HTTP/2 multiplexing that closes
   `agalma-www`.
4. **Nested `block_on` fix (`agalma-ebg.8`).** The kernel server both *serves*
   over Connect and calls its `LedgerClient` synchronously from inside the
   async Connect handler; the framed client was synchronous so this was latent,
   but over Connect `Runtime::block_on` panics ("cannot start a runtime from
   within a runtime"). `agalma_ipc::block_on_nested` (`tokio::task::block_in_place`)
   steps the worker aside so the peer reactor progresses; `ledger_client::call`
   uses it, and the execution client applies it defensively. Other Connect
   clients are not nested today and were left untouched.
5. **Process transport still opt-in / in-process default (tracked:
   `agalma-rvj`, B1-F3).** All boundaries keep `AGALMA_*_TRANSPORT=in-process`
   as the production default; `process` is selectable and test-proven. Under a
   process binding the wire now defaults to Connect. A default flip to process
   requires a live OpenCode process-transport smoke and is deferred.
6. **`ExecutionApi` adapter gap resolved, not residual.** The `agalma-5de`
   B1-F1 gap (`step`/`recover` returning `UnsupportedCapability` over the
   process boundary) is closed by caller-side effect dispatch (commit
   `6e8d69a`); IPCX.7 proves `step`/`recover` and the handler-registry protocol
   faithful over both wires at the engine-op level.
7. **Test-only lint nit.** `cargo build --offline --workspace` is
   warning-free; the `harness_connect` test target emits one unused-import
   warning (`ServerInfo`). Out of this bead's scope (docs + architecture tests
   only); harmless.

## Verdict

All six Tier-1 seams — harness, sandbox, workspace, task queue, ledger, and the
execution/kernel boundary — speak the ConnectRPC envelope over UDS with full
B1-recipe evidence (dual-binding parity, staged rebind, rollback) recorded in
the per-boundary docs above. The `agalma-www` head-of-line constraint is gone
(HTTP/2 multiplexing), contracts and the ledger at-rest JSON are unchanged, and
the Connect stack is confined to `agalma-ipc` at the manifest and source levels
(9 architecture tests green). Builds are offline (`cargo fetch --offline`
empty; `cargo build --offline --workspace` clean). Residual items are tracked
(`agalma-rvj` default flip; the reserved `Stream` RPC for TUI push). Epic
`agalma-ebg` closes on this record.
