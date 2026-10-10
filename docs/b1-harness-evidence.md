# B1.2 — harness extraction evidence

Bead: `agalma-0ht.3`. Design: `docs/b1-extraction.md` (D1–D5, D7). Depends on
`agalma-0ht.2` (B1.1 `agalma-ipc`). HEAD at start: `717a70f`.

Goal: move `HarnessApi` behind a process (Unix-socket) boundary without changing
any contract schema or consumer (phase) logic, and prove replacement through the
declared API with dual-binding parity, staged rebind, rollback, and IPC
conformance.

## Deliverables

| # | Deliverable | Where |
|---|---|---|
| 1 | `HarnessClient` implementing `HarnessApi` over `Client` | `crates/agalma-ipc/src/harness_client.rs` |
| 2 | Reference server bin (profile via `AGALMA_HARNESS_PROFILE`) | `crates/agalma-harness-reference/src/bin/agalma-harness-reference-server.rs` + `src/server.rs` |
| 2 | OpenCode server bin (embeds the in-process sandbox until B1.3) | `crates/agalma-harness-opencode/src/bin/agalma-harness-opencode-server.rs` + `src/server.rs`, `src/config.rs` |
| 3 | `server_command()` + binary resolution relative to the conductor exe | impl crates; `harness_process::resolve_server_binary` |
| 4 | `ProcessBinding` supervisor (spawn, READY+timeout, bind gate, `HarnessClient`, SIGTERM stop, restart) | `crates/agalma-conductor/src/harness_process.rs` |
| 5 | `AGALMA_HARNESS_TRANSPORT=in-process|process` selection | `harness_process::HarnessTransport` |
| 6 | Parity / staged-rebind / rollback / IPC-conformance tests | `crates/agalma-conductor/tests/harness_process.rs` |
| 7 | This evidence doc + invariance proof | `docs/b1-harness-evidence.md` |

### New interfaces

- `agalma_ipc`: `CommandSpec` (`command.rs`); `HarnessClient` + params
  (`RunTurnCall`, `OperationCall`, `SessionCall`, `AttemptCall`) and
  `map_ipc_error` (`harness_client.rs`); `register_harness`, `server_info_from`,
  `ensure_all_handlers`, `HARNESS_METHODS` (`harness_server.rs`);
  `Server::has_handler`; re-exported `socket_from_args`.
- `agalma_harness_reference`: `serve`, `build_server`, `server_command`,
  `profile_from_env`, `profile_env_value`, `scenario_dir_for`, `run_from_args`,
  `SERVER_BIN`, `PROFILE_ENV`, `SCENARIO_DIR_ENV`.
- `agalma_harness_opencode`: `build_config`, `OpenCodeConfigInit`,
  `server_command`, `build_server`, `config_from_env`, `run_from_args`,
  `SERVER_BIN`, `ROOT_ENV`, `PROFILE_PATH_ENV`, `PROXY_URL_ENV`, `PAID_ENV`;
  `OpenCodeHarness::new` now takes `Box<dyn SandboxApi + Send>`.
- `agalma_conductor::harness_process`: `HarnessTransport`, `ProcessBinding`,
  `resolve_server_binary`, `reference_server_spec`, `open_reference_harness`,
  `reference_profile`, `ready_json`, `TRANSPORT_ENV`, `READY_TIMEOUT`.

## Results

All commands from the repo root, `--offline`.

| Command | Observed |
|---|---|
| `cargo test --offline -p agalma-ipc` | `26 passed` (unit) + `12 passed` (conformance) + doc-tests; exit 0 |
| `cargo test --offline -p agalma-conductor` | exit 0; 17 test binaries `test result: ok`, 0 failures |
| `cargo test --offline -p agalma-conductor --test harness_process` | `6 passed; 0 failed` |
| `cargo test --offline -p agalma-conductor --test architecture` | `4 passed; 0 failed` |
| `cargo test --offline` (full workspace) | exit 0; 59 test binaries `test result: ok`, 0 failures |

`tests/harness_process.rs` cases:

| Test | What it proves |
|---|---|
| `shadow_parity_in_process_vs_process_reference` | In-process `ReferenceHarness` vs supervised process `ProcessBinding` produce an identical, timing/ID-free transcript: `Describe`, terminal state label, event `render()` sequence, produced artifact digest, and the bind-gate `BindingRecord` (`binding_id`, implementation/version, capabilities, generation). |
| `staged_rebind_uses_pinned_binding_not_flipped_config` | An in-flight operation pinned to `reference-fail` keeps that pin across a process swap; the next attempt binds the flipped config (`reference`); binding ids differ. Mirrors `tests/rebind.rs` case b. |
| `rollback_restores_binding_without_state_loss` | Candidate binding staged then retired; prior binding restored `Active`; a pre-rollback ledger event survives; the next attempt pins the restored binding. |
| `harness_client_duplicate_id_replay_and_connection_loss` | Raw `Client`: duplicate request id replays the recorded receipt (no second effect); a fresh id advances; post-death call surfaces `Unavailable`/`UnknownOutcome`; missing socket is `Unavailable`. |
| `harness_client_maps_transport_loss_to_contract_error` | `HarnessClient` surfaces a dead transport as a `ContractError` (`UnknownOutcome`/`KnownFailure`). |
| `reference_server_child` | Child entry point (no-op under a normal run). |

The process tests spawn the component server either from the real
`agalma-harness-reference-server` binary (when built, e.g. full-workspace runs) or
by re-exec'ing the test binary as the server (`current_exe reference_server_child
--exact --nocapture`), so a package-scoped `cargo test -p agalma-conductor` run
still exercises the real process boundary. Both paths were run green.

## Default transport decision

`HarnessTransport::from_env()` reads `AGALMA_HARNESS_TRANSPORT`
(`in-process` default, `process` optional). The default is left **`in-process`**.

Deviation (factual, per D5): the build/verify phase code is unchanged, so the
phase path still constructs harnesses in-process and does **not** consult the
transport env var. Flipping the default to `process` would therefore be inert in
this bead and cannot be validated by the suite. The process transport is instead
proven directly by the dedicated boundary tests above, which pass in the full
suite. Wiring the build activity to the transport factory is deferred so
"consumer logic unchanged" (D5) holds; it is the natural B1.3 follow-on when the
sandbox boundary lands and the server socket layout is finalized.

## Deviations and design corrections (noted, not applied to `docs/b1-extraction.md`)

1. **Architecture-test carve-out for the harness server's in-process sandbox.**
   D6 forbids impl→impl normal dependencies, but D2's derived requirement says an
   already-extracted server hosts a not-yet-extracted dependency in-process until
   that dependency is extracted (the harness server uses the sandbox client only
   after B1.3). These conflict for the harness server, which the bead explicitly
   requires to construct `OpenCodeHarness::new(Box::new(SeatbeltSandbox::new()),
   config)`. `tests/architecture.rs` therefore gains one sanctioned, documented
   edge (`agalma-harness-opencode -> agalma-sandbox`) in both the Cargo.toml and
   source-token checks, removed when B1.3 lands. This is a test change under the
   allowed `crates/agalma-conductor/**`; no contract or phase logic changed.

2. **OpenCode server configuration.** The server assembles its config from
   environment (`AGALMA_HARNESS_ROOT`, `AGALMA_HARNESS_PROFILE_PATH`,
   `AGALMA_HARNESS_PROXY_URL`, `AGALMA_HARNESS_PAID_PROXY`, `AGALMA_MODEL`) via
   the shared `build_config` constructor that the conductor's build/verifier paths
   now also call (removing the duplicated provider-override logic). The OpenCode
   process path is not exercised end-to-end in this bead (it needs a live
   `opencode` install and the B1.3 sandbox boundary); only the reference transport
   is driven by tests. The server runs its serial serve loop inside a multithread
   Tokio runtime because sandbox-backed methods require a reactor.

3. **Unix socket path length.** Component sockets live directly under short
   `target/test-runs/hp-<pid>-<n>/` run dirs in tests to stay within `SUN_LEN`
   (~104 bytes). The supervisor already creates the socket's parent directory.

4. **`Cargo.lock`.** Modified only to add the new intra-workspace dependency
   edges (`agalma-ipc` for the conductor, reference, and OpenCode crates). No new
   external crate was added; the lockfile delta is a mechanical consequence of the
   Cargo.toml changes above.

## Consumer-invariance proof

`crates/agalma-conductor/src/phases.rs` changes are limited to construction
sites: `run_live`/`run_verifier_session` now call the shared
`build_config`/`OpenCodeConfigInit` (identical config) and the removed private
`configure_provider_override` helper. Phase dispatch logic, phase ordering,
recovery, and every activity body are unchanged.

`git diff --name-only` (tracked) plus new files, all within the allowed set:

```
Cargo.lock                                                    # intra-workspace dep edges only
crates/agalma-conductor/Cargo.toml
crates/agalma-conductor/src/harness_process.rs                # new
crates/agalma-conductor/src/lib.rs
crates/agalma-conductor/src/phases.rs
crates/agalma-conductor/tests/architecture.rs
crates/agalma-conductor/tests/harness_process.rs              # new
crates/agalma-harness-opencode/Cargo.toml
crates/agalma-harness-opencode/src/adapter.rs
crates/agalma-harness-opencode/src/bin/agalma-harness-opencode-server.rs   # new
crates/agalma-harness-opencode/src/config.rs                  # new
crates/agalma-harness-opencode/src/lib.rs
crates/agalma-harness-opencode/src/server.rs                  # new
crates/agalma-harness-reference/Cargo.toml
crates/agalma-harness-reference/src/bin/agalma-harness-reference-server.rs # new
crates/agalma-harness-reference/src/lib.rs
crates/agalma-harness-reference/src/server.rs                 # new
crates/agalma-ipc/src/command.rs                              # new
crates/agalma-ipc/src/harness_client.rs                       # new
crates/agalma-ipc/src/harness_server.rs                       # new
crates/agalma-ipc/src/lib.rs
crates/agalma-ipc/src/server.rs
docs/b1-harness-evidence.md                                   # this file
```

No file under `crates/agalma-contracts/**` changed; no contract schema changed.
