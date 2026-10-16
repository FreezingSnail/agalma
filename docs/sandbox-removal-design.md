# Sandbox layer removal design

Status: proposed (design, not implemented). Owner: Sol. Implementers: GPT-6 Luna /
DeepSeek Flash. Supersedes the confinement boundary documented in
`docs/b1-sandbox-evidence.md` (that file is a frozen historical receipt and is
**not** rewritten; this document is its amendment).

## 1. Decision

Remove the sandbox layer entirely and replace it with a small in-process
**process launcher**. Concretely:

- delete the `agalma-sandbox` crate (Seatbelt backend, profile, server binary);
- delete the `SandboxApi` contract and the `agalma-ipc` sandbox client/server;
- delete the conductor's sandbox supervisor (`sandbox_process.rs`);
- delete the kernel/harness socket plumbing that only existed to reach the
  sandbox server;
- add one crate `agalma-process` exposing `LocalLauncher`, which owns the
  process-group/lifecycle logic that was previously entangled with the Seatbelt
  profile.

Retained properties: **multiple in-flight children**, **environment
separation**, and a **kill switch** (whole-descendant-tree termination with
evidence). Anything enforced solely by Seatbelt is dropped:

- OS read/write confinement (the protected-oracle read-only root),
- network-egress denial (the loopback proxy is no longer a security boundary),
- the fail-closed profile validate/preflight gate.

The loss of the first two is a deliberate human decision, recorded as an accepted
guarantee reduction in §7. It weakens `docs/sf6-protected-inputs.md` and the
self-mutation contracts from *enforced* to *advisory* properties.

## 2. Motivation

All four stated drivers:

1. **Portability** — `sandbox-exec` and the `.sb` profile are macOS-only, so the
   factory cannot run on Linux/CI without a parallel confinement path.
2. **Maintenance** — `sandbox-exec` is deprecated (S0b open item: re-verify on
   every macOS upgrade); the profile carries seven parameters, an inert-slot
   padding rule, and an xcrun-cache carve-out that must be re-audited.
3. **Complexity** — reaching a `sandbox-exec` wrap required a whole supervised
   component server, a Unix socket, two wire transports, framed-JSON clients, and
   parity tests, all to launch a subprocess.
4. **Dead weight** — the process-group kill, liveness, and env isolation that the
   factory actually depends on are plain Unix and never needed Seatbelt.

## 3. What the layer is today

Three stacked pieces:

| Layer | Pieces |
| --- | --- |
| Backend | `crates/agalma-sandbox`: `seatbelt.rs`, `profile.rs`, `profiles/worker.sb`, `bin/agalma-sandbox-server.rs` |
| Contract seam | `agalma_contracts::sandbox` (`SandboxApi`/`LaunchSpec`/`SandboxChild`); `agalma-ipc::{sandbox_client,sandbox_server}` |
| Wiring | `conductor/sandbox_process.rs` supervisor; call sites in `phases.rs`, `session.rs`, `work.rs`, `concierge.rs`, `review.rs`, `sweep.rs`, `composition.rs`, `workers.rs`, `harness_process.rs`, `execution_process.rs`, `components.rs`; `harness-opencode` adapter; `kernel` `with_sandbox` seam |

### 3.1 Properties and their disposition

| Property | Provided by | Disposition |
| --- | --- | --- |
| Multiple concurrent children | `SeatbeltSandbox.children: HashMap<pid, Child>` | **Keep** (same mechanism in `LocalLauncher`) |
| Environment separation | caller-supplied `LaunchSpec.env` + `env_clear().envs()` | **Keep** (unchanged) |
| Tree kill + `StopEvidence` | `process_group(0)` + `killpg` + `pgrep -g` sweep + `SIGKILL` escalation | **Keep** (lifted from `seatbelt.rs`) |
| Liveness (`alive`) | `/bin/kill -0`, EPERM counts as alive | **Keep** |
| Terminate from pid/pgid only | `terminate` works without an owned `Child` (post-restart) | **Keep** (kernel depends on it) |
| Attempt-scoped `TMPDIR`/`XDG`/`HOME` | caller-driven env, conductor-owned attempt dirs | **Keep** (dirs still reaped by `reap.rs`) |
| OS read/write confinement (deny-default profile) | Seatbelt | **Drop** |
| Protected-oracle read-only root | Seatbelt `PROTECTED` | **Drop** (accepted) |
| Network-egress denial | Seatbelt loopback-only profile | **Drop** (accepted) |
| Fail-closed profile validate/preflight | `profile::validate` + `preflight` | **Drop** |
| Profile parameter substitution + inert slots | `profile.rs` | **Drop** |

## 4. Target architecture

### 4.1 Contract: `agalma_contracts::process`

Rename the `sandbox` module to `process` and trim the types:

```rust
// crates/agalma-contracts/src/process.rs
pub struct ProcessSpec {
    pub program: String,
    pub args: Vec<String>,
    pub cwd: String,
    pub env: BTreeMap<String, String>, // complete child environment
    pub attempt: AttemptId,            // reconciliation key
}

pub struct ProcessChild {
    pub pid: u32,
    pub pgid: u32,                     // == pid (group leader)
    pub attempt: AttemptId,
    pub started_at_unix_ms: u64,
}

pub trait ProcessApi {
    fn launch(&mut self, spec: ProcessSpec) -> Result<ProcessChild, ContractError>;
    fn alive(&self, child: &ProcessChild) -> Result<bool, ContractError>;
    fn terminate(&mut self, child: &ProcessChild) -> Result<StopEvidence, ContractError>;
}
```

Dropped fields: `profile`, `attempt_dir`, `protected_dir`, `sock_dir`,
`extra_ro_roots`, `extra_rw_roots`. `StopEvidence` stays in `agalma_contracts::harness`.
`agalma-contracts` remains pure (types + trait only; no spawning).

The trait is kept (not inlined to a concrete type) so the kernel test doubles
(`FakeLauncher`/`GoneLauncher`/`StubbornLauncher`) can still be injected. It costs
nothing at runtime.

### 4.2 Backend: new crate `agalma-process`

`LocalLauncher` is `SeatbeltSandbox` **minus the profile**:

- `launch(spec)`: `Command::new(spec.program)`, `args`, `current_dir(cwd)`,
  `env_clear().envs(spec.env)`, `process_group(0)`; store the `tokio::Child` in
  `HashMap<u32, Child>`; return `ProcessChild { pid, pgid: pid, attempt, started_at_unix_ms }`.
- `alive(child)`: `/bin/kill -0 <pid>`; EPERM ⇒ alive (mirrors `seatbelt.rs`).
- `terminate(child)`: group `SIGTERM`, explicit per-descendant signal from
  `pgrep -g <pgid>`, reap the owned child when present, verify the group is
  empty, escalate to `SIGKILL`; return `StopEvidence`.
- `run(spec, timeout) -> Result<ProcessOutput, ContractError>`: one-shot
  counterpart used by the verifier (exit code is the verdict); same teardown on
  timeout. `ProcessOutput { exit_code, stdout, stderr }`.

No profile, no `sandbox-exec`, no `preflight`, no `serde`. `/bin/kill` and
`/usr/bin/pgrep` exist on macOS and Linux; this is the portability win.

Dependency direction: `agalma-process` → `agalma-contracts` (+ `tokio`). No IPC.

### 4.3 Deletions

- `crates/agalma-sandbox/` (whole crate) and its workspace member entry.
- `agalma_contracts::sandbox` module + `pub use` re-exports.
- `agalma-ipc/src/sandbox_client.rs`, `sandbox_server.rs` + re-exports in
  `agalma-ipc/src/lib.rs`.
- `agalma-conductor/src/sandbox_process.rs` + `pub mod sandbox_process`.
- `agalma-harness-opencode`: `SANDBOX_SOCKET_ENV`, `connect_sandbox`, the
  `profile_path` config field, `PROFILE_PATH_ENV`, the `extra_ro_roots` config
  field, the `attempt_dir`/`protected_dir`/`sock_dir` constructor arguments, and
  the `agalma-sandbox` dev-dependency.
- `agalma-kernel`: `AGALMA_KERNEL_SANDBOX_SOCKET` (`SANDBOX_SOCKET_ENV` in
  `kernel/src/server.rs`), the env-bound `SandboxClient` build.
- `components.rs`: the `sandbox` component row and `Transports.sandbox`.
- `execution_process.rs`: the `sandbox_socket` parameter and its
  `SANDBOX_SOCKET_ENV` export.
- `scripts/verify-peer-transports.sh`: `sandbox` from the component loop and
  `--test sandbox_process` from the smoke invocation.

## 5. Migration by area

### 5.1 Conductor

- `SandboxSource`/`SandboxProcess` disappear; each activity holds a
  `LocalLauncher` (or `Box<dyn ProcessApi>`) directly.
- `phases.rs`: `SeatbeltSandbox::new()` → `LocalLauncher::new()`; drop the
  `profile`/`attempt_dir`/`protected_dir`/`sock_dir`/`extra_*` fields from the two
  `LaunchSpec` constructions (`phases.rs:1757`, `phases.rs:2281`). The `env` map
  (which already carries `TMPDIR`/`XDG`/`CARGO_*`/`AGALMA_PROTECTED_DIR`/
  `AGALMA_CANDIDATE_DIR`) is unchanged.
- `session.rs`, `work.rs`, `composition.rs`: drop the `worker.sb`
  `PRODUCT_PROFILE` writes; `SandboxSource::ensure()` → a `LocalLauncher`;
  `LiveSession.profile_path` is removed. `review.rs`, `concierge.rs`,
  `sweep.rs` only mention the profile in comments — update those.
- Phase deps (`phases.rs:153,169`): remove `profile_path` and `extra_ro_roots`;
  the harness config (`config.rs:17,32`) likewise drops `profile_path` and
  `extra_ro_roots`.
- `workers.rs`: `SandboxChild` → `ProcessChild`; `reconcile_survivor` logic is
  unchanged (it already only needs `alive`/`terminate`).
- `VerifyActivity.sandboxed` (`phases.rs:1510`) is deleted; the `ActivityMode`-
  driven branch collapses to one path. One-shot verify uses `LocalLauncher::run`
  (timeout + tree-kill); `run_in_process` (`phases.rs:688`) is retired because it
  has no timeout.
- `harness_process.rs`: the in-process harness receives a `LocalLauncher` instead
  of a `SandboxClient`; the `sandbox_socket` init field is removed. The
  process-bound harness server constructs its own `LocalLauncher`.
- `execution_process.rs`: drop the `sandbox_socket` parameter; the kernel server
  no longer receives a sandbox socket.
- `reap.rs`: keep the `verify-runtime`/`xdg-*` scratch reaping (they serve env
  isolation, not confinement); update the naming/comments.
- `provider_proxy.rs` / `provider_auth.rs`: **kept**, not deleted (§7.2).

### 5.2 Harness and kernel

- `harness-opencode/src/adapter.rs`: `Box<dyn SandboxApi + Send>` →
  `Box<dyn ProcessApi + Send>`; `LaunchSpec` → `ProcessSpec`; drop profile/env
  plumbing and the `profile_path` config field.
- `harness-opencode/src/server.rs`: delete `connect_sandbox` and
  `SANDBOX_SOCKET_ENV`; build a `LocalLauncher`.
- `kernel/src/engine.rs`: `Option<Box<dyn SandboxApi + Send>>` →
  `Option<Box<dyn ProcessApi + Send>>`; `with_sandbox`/`has_sandbox`/
  `sandbox_mut` → `with_launcher`/`has_launcher`/`launcher_mut` (or keep the
  method names if that minimizes churn — decide in the bead).
- `kernel/src/ownership.rs`: reconstruct `ProcessChild`.
- `kernel/src/server.rs`: delete `SANDBOX_SOCKET_ENV`; construct a
  `LocalLauncher`.
- Cross-process kill still works: the kernel only needs `alive`/`terminate` from
  a pid/pgid, which is exactly the "terminate without an owned handle" path.

### 5.3 Build / tooling

- `Cargo.toml`: remove `crates/agalma-sandbox`, add `crates/agalma-process`.
- `scripts/verify-peer-transports.sh`: component loop becomes
  `ledger workspace taskqueue kernel`; smoke set drops `--test sandbox_process`
  and the expected-passing count drops `5 → 4`.
- `scripts/md-to-html.mjs`: not owned by these beads — another session holds
  staged docs-tooling work there. Regenerating the HTML set (frozen
  `b1-sandbox-evidence.md` entry included) is deferred until that work lands.

## 6. Interfaces deliberately preserved

- `terminate` remains valid from pid/pgid alone (no owned `Child`), because the
  kernel reclaims survivors across a process boundary and after restart.
- `alive` keeps the EPERM-is-alive rule.
- `reconcile_survivor` keeps writing `<run>/stop/<phase>-<attempt>.json` evidence.
- Attempt-scoped `TMPDIR`/`XDG`/`HOME`/`CARGO_*` env construction is untouched.
- The provider proxy stays functional (only its security claim changes).

## 7. Accepted guarantee reduction

### 7.1 Protected-oracle read-only (SF.6)

`docs/sf6-protected-inputs.md:44-49` requires the protected judge to be
materialized in a read-only root outside every candidate-writable root, and
requires proof the runner "cannot write protected files." Today that is enforced
by Seatbelt. After removal it is **advisory only**: the runner still receives
`AGALMA_PROTECTED_DIR` and the frozen commands still judge behavior, but a
candidate is no longer prevented from tampering with the judge root.

Doc amendment: `sf6-protected-inputs.md` gains an amendment stating the
read-only property is convention-only, and `sf_scope_guard.rs` is rewritten to
stop asserting OS-enforced read-only isolation (`sf_scope_guard.rs:710`,
`:833+`, `:1652`, all of which import `SeatbeltSandbox`/`PRODUCT_PROFILE`).

### 7.2 Network egress

The loopback proxy was justified by the Seatbelt loopback-only profile. After
removal the harness can reach the network directly. `provider_proxy.rs` is
**kept** (it still centralizes provider credentials/audit), but it is no longer a
security boundary. `provider_proxy_live.rs`'s "direct network is denied"
assertion is dropped; the proxy behavior test is retained.

### 7.3 Fail-closed launch gate

`profile::validate`/`preflight` disappear; the only launch gate becomes spawn
failure. This removes a deliberate fail-closed check but was only meaningful
because a profile existed.

## 8. Test plan

### Delete

- `crates/agalma-sandbox/tests/sandbox.rs` (whole crate goes).
- `crates/agalma-ipc/tests/sandbox_connect.rs`.
- `crates/agalma-conductor/tests/sandbox_process.rs`.
- `crates/agalma-conductor/tests/harness_sandbox_wire.rs`.

### Rewrite (guarantee-bearing)

- `agalma-conductor/tests/sf_scope_guard.rs` — remove `SeatbeltSandbox`/
  `PRODUCT_PROFILE`; keep the behavioral protected-command judging; drop the
  OS-read-only isolation claim.
- `agalma-conductor/tests/provider_proxy_live.rs` — keep the proxy test, drop the
  egress-denial assertion.
- `agalma-conductor/tests/cargo_home.rs`, `tests/xcrun.rs` — drop the
  `PRODUCT_PROFILE` write.
- `agalma-conductor/tests/designer_payload.rs` — replace the `register_sandbox`
  mock server with a `LocalLauncher`/`ProcessApi` double.
- `agalma-conductor/tests/sf_cancel.rs`, `tests/sf_recovery_gate.rs` — rename the
  `GoneSandbox` fake and drop profile setup.
- `agalma-kernel/tests/support/sandbox.rs` → `support/process.rs`; update
  `sf_cancel_kernel.rs`.
- `agalma-conductor/tests/architecture.rs` — drop `agalma-sandbox` from
  `IMPL_CRATES` and the token-scan self-test string; add `agalma-process` as a
  non-impl crate (it is a backend, like the old sandbox crate, so it may still be
  listed as an impl crate — decide in the bead).
- `agalma-harness-opencode/tests/live_smoke.rs` — replace `SeatbeltSandbox`/
  `PRODUCT_PROFILE` with `LocalLauncher`; `tests/wire_conformance.rs` — drop the
  sandbox-wire agreement case.
- `agalma-conductor/src/components.rs` unit tests and `src/session.rs` tests —
  update for the removed sandbox row / profile-materialization assertion.

### New — `agalma-process` property tests (saved in the repo)

- **Concurrency:** launch N long-running children; all report alive; terminate
  all; each returns `process_group_gone`.
- **Env separation:** child sees exactly the supplied `env` (nothing inherited);
  a child given no `TMPDIR` does not see the parent's.
- **Tree kill:** parent spawns a grandchild; terminate the parent; assert the
  grandchild is gone and `StopEvidence.process_group_gone` holds.
- **Terminate-from-pid-only:** build a `ProcessChild` from pid/pgid alone (as
  after a restart) and terminate it; no owned handle.
- **One-shot timeout:** `run(spec, short)` on a sleeper returns `KnownFailure`
  and leaves no survivor.
- **Alive semantics:** `alive` false after normal exit.

Each property test records a deterministic generation scheme (small fixed N,
fixed sleep durations) and asserts independently of the implementation.

### Not re-run

Historical acceptance receipts that referenced the sandbox boundary remain as
frozen evidence; no frozen receipt is rewritten.

## 9. Bead breakdown

Each bead merges green; one bead = one implement/verify loop. P2 and P3 are
independent (different crates, disjoint file ownership) and run in parallel from
the same base after P1.

| # | Bead | Content | Depends |
| --- | --- | --- | --- |
| P1 | Launcher foundation | Add `agalma-process` crate + `agalma_contracts::process`. Old sandbox untouched. Include the property tests of §8. | — |
| P2 | Conductor + harness migration | Switch all conductor + harness-opencode source and tests to `LocalLauncher`/`ProcessApi`; delete `sandbox_process.rs`; drop profile writes, `profile_path`, `extra_ro_roots`, `sandboxed`; update `components.rs`/`harness_process.rs`. The guarantee-bearing conductor tests (`sf_scope_guard`, `provider_proxy_live`, `cargo_home`, `xcrun`, `designer_payload`, `sf_cancel`, `sf_recovery_gate`) are rewritten here because the sandbox types vanish; the accepted-loss wording goes into the test comments, formal doc amendments in P5. | P1 |
| P3 | Kernel + execution migration | `kernel` engine/ownership/server → `ProcessApi`/`LocalLauncher`; `ownership` reconstructs `ProcessChild`; drop `AGALMA_KERNEL_SANDBOX_SOCKET`; `execution_process.rs` drops the `sandbox_socket` parameter and its tests; kernel test doubles renamed. | P1 |
| P4 | Deletion + lint + handoff regression | Remove `agalma-sandbox` crate + workspace member, `agalma-ipc` sandbox client/server, `agalma_contracts::sandbox`, dev-deps, `scripts/verify-peer-transports.sh` entry (5→4 smokes), `md-to-html` entry; update `architecture.rs` (`agalma-sandbox` out, `agalma-process` documented as infrastructure like `agalma-ipc`); add the handle-less terminate regression (§10); run `peers` + one `full`. | P2, P3 |
| P5 | Docs + amendment | Write the superseding amendment notes; amend live docs (`sf6-protected-inputs`, `self-mutation-contracts`, `architecture`, `component-contracts`, `testing-strategy`, `development-workflow`, `tui-design`, `smart-routing-design`, `paid-route-evidence`, `README`, `m0-skeleton`); frozen receipts (`b1-sandbox-evidence`, `b1-exit-evidence`, `ipc-exit-evidence`, other `b1-*`, `m0/m1-exit-evidence`) get a superseding note only, never rewritten; record launcher evidence. | P4 |

Workflow per `AGENTS.md`/`docs/development-workflow.md`: each bead in a named
worktree `target/worktrees/<bead>` on `task/<bead>`, redirected cache
`target/worktree-caches/<bead>/target`, commit `<what> (agalma-<bead>)` after
review.

## 10. Risks and open questions

Resolved at filing (2026-10-15):

- **`agalma-process` placement:** new workspace crate (bead `agalma-ib2`).
- **`IMPL_CRATES` classification:** `agalma-process` is infrastructure, like
  `agalma-ipc` — deliberately not in `IMPL_CRATES`, because `agalma-kernel` and
  `agalma-harness-opencode` (impl crates) depend on it as a normal dependency.
- **`md-to-html.mjs`:** deferred. Another session holds staged docs-tooling work
  (`Makefile`, `scripts/md-to-html.mjs`, `docs/*.html`); the removal beads do not
  own those files, and regeneration happens after that work lands.
- **Kernel rename:** `with_launcher`/`has_launcher`/`launcher_mut` (bead
  `agalma-wmk`).

Remaining risks:

- **Guarantee loss is real, not cosmetic.** SF.6's read-only judge and the
  egress boundary are removed. Downstream self-mutation policy assumes a
  confinement boundary; the amendment must be explicit that this is now
  convention. If confinement is later required for portability, a Linux
  namespace/bubblewrap backend could be reintroduced behind `ProcessApi` without
  re-adding the IPC/server seam.
- **Cross-process ownership.** With one launcher per process (conductor, harness,
  kernel) instead of one shared server, termination relies entirely on pid/pgid,
  which the current code already supports. The harness transport defaults to
  `Process` (`AGALMA_HARNESS_TRANSPORT`), so the production harness server owns
  its `LocalLauncher` and the OpenCode child; the conductor keeps its own for
  build-probe/verify. The P4 bead adds the explicit regression: a fresh
  `LocalLauncher` terminates a child whose owning handle was dropped
  (pid/pgid-only path across processes/restarts).

## 11. Out of scope

- Removing the provider proxy (`provider_proxy.rs`), which is retained.
- Adding a replacement confinement backend.
- Any change to ledger, workspace, taskqueue, or execution boundaries beyond the
  sandbox references listed above.
