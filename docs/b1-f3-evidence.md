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
