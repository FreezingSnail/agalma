# B1-F3 — live OpenCode process-transport smoke + default flip

Bead: `agalma-rvj` (B1-F3). Epic: `agalma-0ht` (B1). Date: 2026-10-11.
Base HEAD: `79e3b42`. Design refs: `docs/b1-exit-evidence.md` (§Limitations 3),
`docs/b1-harness-evidence.md`, `docs/paid-route-evidence.md`,
`docs/m1-exit-evidence.md`.

B1 left every Tier-1 boundary defaulted to `in-process`; process transport was
proven only by the offline boundary tests. `docs/b1-exit-evidence.md` §3 tracked
the missing *live* OpenCode process-transport smoke as the gate before any
default flip. This is that smoke, plus the resulting default-flip decision.

## 0. Result

The live smoke ran the conductor against a scratch clone with
`AGALMA_HARNESS_TRANSPORT=process`, drove the OpenCode harness **across the
process boundary**, and **merged on `main` with real cash cost**:

| Field | Value |
|---|---|
| Scratch clone | `target/test-runs/b1-f3/run-1/target` (`git clone --no-hardlinks`, `79e3b42`) |
| bd store | isolated `.beads` in the clone (`bd init --prefix b1f3`); live store never read/written |
| Seed task | `b1f3-z92` (data/docs; `directive/v0`; budget $2.00; max_attempts 2; scope-guard) |
| Harness transport | `process` (`AGALMA_HARNESS_TRANSPORT=process`) |
| Harness impl | `opencode@opencode-adapter/v1`, binding `binding:opencode:1` (generation 1, `Active`) |
| Model / route | `opencode-go/deepseek-v4.1-flash` / `builder:feature` |
| Execution | `exec:b1f3-z92:1` |
| Attempts | 1 (0 stalls) |
| Verdict | **green / done** (bd `closed`, `close_reason = done: green`) |
| Acceptance | 2/2 exit 0 (marker grep + scope guard) |
| Tokens | 19 665 (18 316 in / 1 349 out) |
| `cash_usd` | **0.005782104** (`quota_usd` 0.0035568) |
| Wall | 44.4 s |
| Candidate = result | `9fbf85a` = `main`, tag `m1/exec-b1f3-z92-1` |
| Files touched | `docs/b1f3-smoke.md` (added) |

## 1. Command

Run from the clone; `--repo` the clone, `--state-dir` outside it, live mode
(no `AGALMA_ACTIVITY_MODE`). The paid route follows `docs/paid-route-evidence.md`
§1; the `--model` pin forces the H3 paid builder turn.

```text
AGALMA_HARNESS_TRANSPORT=process AGALMA_ALLOW_PAID=1 AGALMA_PROXY_LOG=1 \
  AGALMA_IDLE_TIMEOUT_SECS=240 \
  cargo run --offline -p agalma-conductor -- work \
    --repo target/test-runs/b1-f3/run-1/target \
    --state-dir target/test-runs/b1-f3/run-1/state \
    --model opencode-go/deepseek-v4.1-flash \
    --harness opencode --max-tasks 1 --once
```

Seeded issue (`b1f3-z92`), fenced `directive/v0`:

```yaml
directive: directive/v0
target: agalma
ref: main
acceptance:
  - sh -c 'grep -q "b1f3-process-smoke" docs/b1f3-smoke.md'
  - sh -c '! git diff --name-only HEAD~1 HEAD | grep -qE "Cargo\.toml$|/src/.*\.rs$"'
budget_usd: 2.00
priority: normal
family: feature
max_attempts: 2
```

## 2. Process-boundary evidence

The run took the `HarnessTransport::Process` branch of
`harness_process::open_opencode_harness`: the conductor spawns the
`agalma-harness-opencode-server` component server, waits for its `READY`
handshake, runs the bind gate, and calls it over the framed `HarnessClient`
socket. Direct artifacts:

- `state/harness-build-1.sock` — the process-bound harness server socket
  (`<state>/harness-build-<attempt>.sock`), created **only** by the process
  branch (`ProcessBinding`/`spawn_supervised`). The in-process branch connects
  a sandbox client and constructs `OpenCodeHarness` directly, with no such
  socket.
- `state/sandbox.sock` — the sandbox component server (B1.3), always process.
- Ledger `component_bindings` row: `binding:opencode:1`,
  `implementation=opencode`, `implementation_version=opencode-adapter/v1`,
  `generation=1`, `state=active` — the announced server identity accepted by the
  process bind gate.
- Ledger `harness_binding` event pins that binding for
  `op:exec:b1f3-z92:1:build@1`.

The full explicit launch config (`agalma-tqf`) crossed the boundary inside
`AGALMA_HARNESS_LAUNCH_CONFIG`; the server reconstructed the identical sandbox
`LaunchSpec` (profile, attempt/protected/sock roots, model, proxy URL,
paid-provider flag, extra env/roots), so the confined `opencode` launch matched
the in-process one. Proxy path evidence (stderr):

```text
agalma: paid provider credential present (host=opencode.ai, fingerprint=65f71dcdab6a9cef)
provider-proxy: allow CONNECT models.opencode.ai:443
provider-proxy: allow CONNECT opencode.ai:443
provider-proxy: allow POST opencode.ai:443   (×N; OpenCode tool-loop turns)
```

Usage event (builder turn), from the ledger:

```text
phase=build role=builder model=opencode-go/deepseek-v4.1-flash route=builder:feature
tokens_in=18316 tokens_out=1349 cost_usd=0.005782104
cash_usd=0.005782104 quota_usd=0.0035568
```

## 3. Default-flip decision

**Deferred, with the live gate satisfied.** The live OpenCode process route is
proven (§2), so the *only* thing standing between B1-F3 and the flip is a
downstream integration defect that the flip exposes. Flipping
`HarnessTransport::from_env` to default `Process` turns the **deterministic
scripted reference path** (`phases::run_reference`) into a process spawn, and the
reference server socket it opens is
`<state_dir>/harness-ref-<attempt>.sock`. In the test scratch dirs (and any
long production state dir) that path exceeds `SUN_LEN` (~104 bytes), so the
server cannot bind:

```text
agalma-harness-reference-server: serve …/state/harness-ref-1.sock:
  path must be shorter than SUN_LEN
test …rebind… FAILED: step: KnownFailure("server did not announce READY within 20s")
```

With the default flip applied, `cargo test --offline -p agalma-conductor --test
rebind` went **4 failed / 1 passed**; with the default unchanged it is green.
This is a latent path-length defect on the reference process boundary, not a
fault in the OpenCode route the smoke exercised (the smoke's state dir was short
enough that `<state>/harness-build-1.sock` bound). Because the mandated gate
cannot pass with the flip, the **default is left `in-process`** and the flip is
deferred to a follow-up that shortens the reference socket path (a short
per-run socket dir, as the `harness_process` boundary tests already do) — the
`HarnessTransport` enum, the `process` selection, and the single env-default
site are unchanged from B1.

### Gate status (default unchanged)

- `cargo test --offline -p agalma-harness-opencode` — **55 passed; 0 failed**.
- `cargo test --offline -p agalma-conductor --lib` — **113 passed; 0 failed**.
- `cargo test --offline -p agalma-conductor --test rebind` (with an explicit
  `AGALMA_HARNESS_TRANSPORT=process`) — fails on the `SUN_LEN` socket path above;
  this reproduces the flip's blocker directly.
- `cargo test --offline -p agalma-conductor` (full) — additionally blocked at
  the time of this run by a **concurrent worker** mid-migration of the same file
  (`ProcessBinding::spawn` gained a `Wire` argument under `agalma-ebg.2`, with
  `tests/designer_payload.rs` not yet updated). Re-run once that edit lands.

### Recommended follow-up (not applied here)

Shorten the reference/index socket paths in `phases::run_reference` (and the
OpenCode build/verify sockets, which share the same `<state_dir>/…sock` shape) to
a bounded per-run directory, then flip `HarnessTransport::from_env`'s default to
`Process` and re-run both suites.

## 4. Artifacts (scratch, never committed)

- Clone: `target/test-runs/b1-f3/run-1/target` (isolated `.beads`).
- State/ledger: `target/test-runs/b1-f3/run-1/state/ledger.sqlite`.
- Digest: `…/state/runs/exec-b1f3-z92-1/artifacts/digest.json`.
- Acceptance outputs: `…/artifacts/accept@1-0.txt`, `…/accept@1-1.txt`.
- Runner log: `target/test-runs/b1-f3/run-1/work.log`.
- Seed body: `target/test-runs/b1-f3/run-1/task.md`.

No commit/push was performed in the live repo; all bd, merge, and tag activity
happened in the clone.

## 5. agalma-ud1 — default flip to `process` + live Connect-wire smoke (GREEN)

Date 2026-10-12, base HEAD `69db198` (agalma-3ej; on top of agalma-acs). Full
detail: `target/worker-output/agalma-ud1.md`.

The B1-F3 default flip is **applied and proven live** on the Connect wire. Both
prior blockers are cleared: agalma-acs bounded the harness socket paths under
`SUN_LEN`, and agalma-3ej aligned the harness server's outbound sandbox client
onto the env-selected Connect wire.

### 5.1 Result

The conductor ran against a scratch clone with **default transport** (no
`AGALMA_HARNESS_TRANSPORT`) and **default wire** (no `AGALMA_IPC_WIRE`),
drove the OpenCode harness across the process boundary, and **merged on `main`
with real cash cost**:

| Field | Value |
|---|---|
| Scratch clone | `target/test-runs/ud1/run-2/target` (`git clone --no-hardlinks`, `69db198`) |
| bd store | isolated `.beads` (`bd init --prefix ud1`); live store never written |
| Seed task | `ud1-7re` (docs; `directive/v0`; budget $2.00; max_attempts 2) |
| Harness transport | `process` (**default — env unset**) |
| Wire | `connect` (**default — `AGALMA_IPC_WIRE` unset**) |
| Model / route | `opencode-go/deepseek-v4.1-flash` / `builder:feature` |
| Execution | `exec:ud1-7re:1` |
| Attempts | 2 (1 idle stall on attempt 1, attempt 2 green) |
| Verdict | **green / done** (bd `closed`, `close_reason = done: green`) |
| Acceptance | 2/2 exit 0 (marker grep + scope guard) |
| Tokens | 33 030 in / 3 371 out |
| `cash_usd` | **0.011532468** (`quota_usd` 0.0069771) |
| Wall | 669.1 s |
| Candidate = result | `669dccd` = `main`, tag `m1/exec-ud1-7re-1` |
| Files touched | `docs/ud1-smoke.md` (added) |

### 5.2 Command

```text
AGALMA_ALLOW_PAID=1 AGALMA_PROXY_LOG=1 AGALMA_IDLE_TIMEOUT_SECS=420 \
  cargo run --offline -p agalma-conductor -- work \
    --repo …/ud1/run-2/target --state-dir ../state \
    --model opencode-go/deepseek-v4.1-flash \
    --harness opencode --max-tasks 1 --once
```

Process-boundary evidence: `state/harness-build-1.sock` and
`…-2.sock` (created only by the process branch), `state/sandbox.sock`, and the
paid proxy path (`provider-proxy: allow POST opencode.ai:443` × many turns).

### 5.3 Flip + follow-on fixes

The flip is a single site (`HarnessTransport::from_env`: default `Process`,
explicit `in-process` override preserved; doc comments and the unit test
updated).

Completing the flip's production path also required two fixes that agalma-3ej
left out (both exercised by the live smoke):

1. `crates/agalma-ipc/src/sandbox_client.rs` — the Connect backend called a raw
   `Runtime::block_on` per request. The harness server both *serves* over
   Connect and drives the sandbox client from inside its async handler, so the
   call panicked with "Cannot start a runtime from within a runtime" (the live
   run failed `h2 send failed: http2 error`). Routed through the existing
   `block_on_nested` helper the harness/ledger/execution clients already use.
2. `crates/agalma-conductor/tests/designer_payload.rs` — the process-transport
   mock sandbox served legacy framed JSON; with 3ej's Connect client the mock
   now serves the matching wire per test (`serve_wire` + a readiness probe).

### 5.4 Gate status (default = process)

- `cargo build --offline --workspace` — clean.
- `cargo test --offline -p agalma-conductor` — **305 passed; 0 failed**
  (includes `rebind` 5/0 and the process-transport `designer_payload`).
- `cargo test --offline -p agalma-ipc -p agalma-harness-opencode -p agalma-sandbox`
  — 165 passed; 0 failed.

### 5.5 Peer boundaries (deferred)

`AGALMA_{LEDGER,WORKSPACE,TASKQUEUE,EXECUTION}_TRANSPORT` remain `in-process`
default. Deferred to **`agalma-fez`**: their sockets still use the raw unbounded
`<state_dir>/<name>.sock` shape (only the harness socket was bounded by
agalma-acs — the always-process sandbox socket is also unbounded), and none has
a live process smoke. Flip each only after the bounded-socket scheme is
generalised and a live smoke per boundary proves the production path.

## 6. agalma-fez — peer boundary flips: bounded sockets + live smokes (GREEN)

Date 2026-10-12, base HEAD `3ddcd98` (on top of agalma-ud1). Design frozen by
the orchestrator. Full detail: `target/worker-output/agalma-fez.md`.

The peer boundaries now share the bounded socket scheme and their transport
defaults are flipped where a production consumer exists. `ledger`, `workspace`,
and `taskqueue` default to `Process` (mirroring the agalma-ud1 harness pattern:
`Some("in-process") => InProcess, _ => Process`). `execution` keeps `InProcess`
(deferral — no production consumer). `sandbox` has no transport selector and is
always process.

### 6.1 Per-boundary result

| Boundary | Bounded socket | Live process smoke | Default after flip | Notes |
|---|---|---|---|---|
| ledger | `ledger_socket` → `bounded_socket_path` | `live_process_smoke_ledger_long_state_dir` | **`Process`** | spawn + handle + lib.rs signal handler all use `ledger_socket` |
| workspace | `WorkspaceProcess::spawn` | `live_process_smoke_workspace_long_state_dir` | **`Process`** | `WorkspaceHandle::process` spawns through it |
| taskqueue | `TaskQueueProcess::spawn` | `live_process_smoke_taskqueue_long_state_dir` | **`Process`** | `TaskQueueHandle::process` spawns through it |
| execution | `execution_socket` | `live_process_smoke_execution_long_state_dir` | `InProcess` (**deferred**) | kernel binding is a candidate with **no production consumer**; a flip would change nothing on the production path |
| sandbox | `SandboxSource::socket` | `live_process_smoke_sandbox_long_state_dir` | n/a (always process) | no transport selector; bounded now |
| session harness | `session_harness_socket` (Process only) | `session_harness_socket_is_bounded_for_long_state_dirs` (unit) | `Process` (agalma-ud1) | designer/concierge/reviewer harness socket is bounded on the production path |

Each smoke spawns the **real** server through the production spawn path
(`LedgerProcess::spawn` / `WorkspaceProcess::spawn` / `TaskQueueProcess::spawn` /
`ExecutionProcess::spawn` / `SandboxSource::ensure`) under a **long** state dir
(so the direct `<state_dir>/<name>.sock` path exceeds `SUN_LEN`), round-trips one
real call, and asserts the socket file exists at `<state_dir>/<name>.sock`
through the alias. Smokes are ungated, local-only, and never mutate the process
environment (defaults are asserted by unit tests; smokes run with the explicit
`Process` path).

### 6.2 Alias mechanics

`crate::phases::bounded_socket_path(state_dir, name)` returns the direct
`<state_dir>/<name>` when `as_os_str().len() < SUN_LEN_LIMIT` (104), else
`<temp_dir>/agalma-sock/<8hex-state-hash>/<name>`. The bounded parent is a
**symlink to the state dir**, so the socket file still lands in `state_dir` —
components.rs liveness/watch/GC (`state_dir.join("<name>.sock").exists()` and the
`harness*.sock` glob) stay valid. Alias construction failure falls back to the
direct path (unchanged semantics; the caller reports the bind error).
`harness_socket_path` delegates: `InProcess` → direct, `Process` →
`bounded_socket_path`. Unit tests: `process_harness_socket_path_is_bounded_for_long_state_dirs`
(long dir canonicalize proof, short dir direct, in-process direct) and
`bounded_socket_path_reuses_alias_and_bounds_long_dirs` (alias reuse/stable base).

### 6.3 Required follow-on fixes (agalma-ipc)

Flipping the defaults exposed two latent defects on the Connect client path that
the in-process defaults had masked (the same class agalma-ud1 fixed for the
harness/sandbox clients):

1. **Nested-runtime connect/call.** `ledger`/`workspace`/`taskqueue`/`execution`
   `connect_wire` and `workspace`/`taskqueue` `call` used a raw `Runtime::block_on`
   from inside the conductor's Tokio runtime → "Cannot start a runtime from within
   a runtime". Routed through the existing `block_on_nested` helper (mirrors
   harness/sandbox clients).
2. **Blocking runtime drop.** `ConnectBackend` dropped a multithread `Runtime`
   inside the async conductor → "Cannot drop a runtime in a context where blocking
   is not allowed". Added the agalma-ud1 `runtime: Option<Runtime>` +
   `shutdown_background()` `Drop` to the four clients.
3. **Orphaned supervised servers on conductor `SIGKILL`.** A supervised server
   outlived a hard-killed conductor and kept its state-dir lock, so a restart
   (`recovery` case 1/4) could not rebind. `spawn_supervised` now spawns the
   server with a **piped stdin** held open for the server's lifetime and sets
   `AGALMA_SERVER_PARENT_DEATH_STDIN`; `agalma_ipc::serve_wire` watches stdin for
   EOF and exits, so a server never outlives its supervisor. (Also fixed the
   signal-handler wire: `LedgerClient::connect` now uses the env-selected wire —
   Connect default — matching the server, so the kill latch persists under the
   flipped default.)

### 6.4 Commands + results

```text
cargo test -p agalma-conductor --test ledger_process      # 11 passed; 0 failed
cargo test -p agalma-conductor --test workspace_process   # 8 passed; 0 failed
cargo test -p agalma-conductor --test taskqueue_process   # 9 passed; 0 failed
cargo test -p agalma-conductor --test execution_process   # 11 passed; 0 failed
cargo test -p agalma-conductor --test sandbox_process     # 8 passed; 0 failed
cargo test -p agalma-conductor --lib                      # 153 passed; 0 failed
cargo test -p agalma-ipc                                  # 49 + 14 + 9 + … passed; 0 failed
cargo test -p agalma-conductor                            # (full) green with the new defaults
cargo build --workspace --offline                         # clean
```

### 6.5 Deferrals

- **Execution default flip** — the kernel binding is a candidate with no
  production consumer; `ExecutionTransport::from_env` keeps `InProcess`. The
  socket is bounded and the smoke proves the spawn path, so the flip can follow
  once a production caller selects the kernel binding.
- **`docs/self-hosting-roadmap.md` and `docs/self-mutation-*.md`** — carry
  uncommitted user edits; not touched by this bead.

### 6.6 Sol repair verification and cold executable launches

The required five smoke cases now fail when their real server binary is missing;
they report the binary, state directory, addressed socket, wire and PID, require
an addressed path shorter than 104 bytes, and check server cleanup. Execution
also checks its ledger's stop evidence; sandbox checks both its launched child
and server teardown. Six isolated missing-binary checks passed, including a
kernel-present/ledger-missing execution case. They are recorded in
`target/test-runs/fez-repair/results.json`, with the earlier smoke failures retained.

Shared startup diagnostics distinguish stdout EOF/read failure from an actual
READY timeout and include executable, socket, PID and exit status. The new EOF
regression case checks that failed startup reaps the child. The production
READY timeout remains 20 seconds.

The raw verification run reproduced READY timeouts, so those failures are not
counted as passing evidence. The captured sandbox failure provides a concrete
host-side explanation for that instance: PID 94623 launched at
2026-10-12 22:30:16.176 EDT; its sample contains 779 observations at
`_dyld_start + 0`, before Rust `main`. macOS execution-policy logs assess the
same executable at 22:30:30.828 and report its provenance at 22:30:44.590,
after the service deadline. This supports execution assessment as the delay in
that cold launch; it does not establish the cause of every historical timeout.
Artifacts: `sandbox-1-server-94623-sample.txt`, `sandbox-smoke-1.log`, and
`sandbox-execution-policy.log` under `target/test-runs/fez-repair/`.

The explicit offline verification recipe is:

```sh
bash scripts/verify-peer-transports.sh
```

It builds the workspace binaries, records their SHA-256 hashes, invokes each
peer executable without arguments and requires its missing-socket rejection,
then runs the five required smokes and the full conductor suite with transport,
wire and paid-live overrides unset. The argument-parser preflight has no socket
or task effects and lets host assessment finish before the measured service
startup. It changes no macOS security settings. The runner rejects zero executed
smokes and skipped cases, and propagates a failed test command.

Final outcome and counts are in `docs/fez-acceptance-receipt.json`; the complete
log is `target/test-runs/fez-repair/qualified-acceptance.log`. Qualified acceptance
does not claim that a previously unassessed executable can always start within
20 seconds. Binary-install qualification belongs in the later plugin lifecycle;
durable cancellation and descendant settlement remain SF.4/SF.5 work.

Historical stopping point: the user requested a stop to conserve usage. The qualified run
passed all five required smokes and 231 conductor checks without failures, then
was interrupted with exit 130 before the full suite completed. This supersedes
any full-green claim in §6.4 for that interrupted run. The original receipt is
retained as `docs/fez-interrupted-receipt.json`.

Resumed 2026-10-13: all recorded FEZ source hashes matched the checkpoint, while
the rebuilt server binaries were explicitly preflighted and smoke-tested again.
All five fresh smokes passed. The saved checkpoint and resumed targets together
cover every conductor unit/integration target: **335 passed, zero failed, two
live-provider tests ignored**. The resumed command exited 0, and all five server
hashes remained stable. Doctests were not rerun in this resumption. The
consolidated receipt is `docs/fez-acceptance-receipt.json`; the resumed output is
`target/test-runs/fez-repair/resumed-acceptance.log`. FEZ is closed, and no
unit/integration verification targets remain outstanding.
