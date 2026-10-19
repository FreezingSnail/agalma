# Plugin platform evidence — HP.8 independent plugin proof

Bead: `agalma-jmr.8` (HP.8). Status: **evidence complete — focused 13/13,
architecture 9/9, and the required full workspace acceptance PASS on the
integrated `main` (`d62afd4`).** HP.8 is not committed or closed by this session
(pending Sol re-review); no acceptance criterion is changed. HP.6 has
merged (`d62afd4`, live host health reaches the shipped watch UI), and the
focused proof now drives the **real supervisor crash/recovery** path: it kills
the child, exercises `PluginHost::supervise_tick` (which observes the exit and
schedules the backoff restart), and reads `PluginHost::plugin_components()` --
the same panel the shipped watch tick overlays -- asserting
`failed`/`disconnected` → `ready`. No test-side
`record_health(Unavailable)`/`confirm_ready_health` synthesizes the failure, and
the rendered `App::overlay_plugins`/`render_to_string` frame is asserted
(`DISCONNECTED` on crash, `health: ready` on recovery). The Sol-review repairs are
in: case 5 now drains/stages g2 and proves unresolved pinned g1 work blocks
removal (`EffectUnconfirmed`) before reconcile and activation; cases 7/8 carry
hub-digest guards; case 9 fully tears down the first host (runtime, child, host,
probe, action service, action driver, selected ledger) and reassembles a fresh
runtime/host/ledger, then compares the full durable record and proves no repeated
effect. The required full workspace acceptance (architecture included) is
recorded below. HP.8 owns no production file. All prior run receipts, including
the preserved `SUN_LEN` failure, are retained unchanged.

Evidence root: `/Users/connorfranc/code/agalma/target/test-runs/agalma-jmr.8/`.
Implementation worktree: `/Users/connorfranc/code/agalma/target/worktrees/agalma-jmr.8`.
Branch: `task/agalma-jmr.8`, based on `main` at
`d62afd460e4e26e82e3cb81f8346ca8d11bdf115` (HP.6 live host health overlay over
the shipped watch UI; earlier `6b685e0` added the HP.4 owned-launch health
identity pin and typed RPC health observation; `3eabeed` restored durable
registry generations; `27056f1` added configured-root auto-install).
Redirected workspace cache: `target/worktree-caches/agalma-jmr.8/target`.
Fixture compiler cache: `target/plugin-pack-build` (persistent, isolated).

## What the proof drives

The focused proof (`crates/agalma-conductor/tests/hp_plugin_proof.rs`) drives the
**real production host wiring**, not a test-only hub:

* Install / startup / shutdown: the shipped `agalma plugins --once` binary
  (`CARGO_BIN_EXE_agalma`).
* Reporting, generic read model, actions, restart and replacement: the production
  `PluginHost::assemble_with_ledger` (the exact function `run_plugin_host` calls),
  including serving the production hub capability socket with
  `plugin_router::serve_hub`, a real out-of-process fixture plugin
  (`fixtures/plugin-pack`) over a Unix Connect socket, and the **same selected
  ledger** (`LedgerHandle`) the read model and action service use.
* The live plugin panel is read through the shipped
  `PluginHost::plugin_components()` (HP.6) — the exact panel `run_plugin_host`'s
  watch tick overlays onto the shipped UI — so the crash/recovery assertions run
  against the production read model, not a re-implementation.
* Actions run through the production `ServiceActionDriver` +
  `PluginRpcActionExecutor` and are dispatched through the actual
  `watch::App::handle_key_with` UI key path (`a`), against a real
  `ActionService` over the selected ledger. No mock action driver and no
  private-assembly hub are used.
* Crash/restart (cases 4/4b): the real `PluginHost::supervise_tick` observes the
  killed child (`reap_exited` → `handle_crash` → `registry.mark_failed`), and the
  live `PluginHost::plugin_components()` panel reports `failed`/`disconnected`
  for the pinned generation; the next due tick restarts the child under backoff
  and the identity-checked `RpcHealthProbe` gate inside `supervise_tick` restores
  `ready` — with no test-side `record_health(Unavailable)`.
* Upgrade/rollback and hub reassembly (cases 5/8/9) drive the production
  `PluginHost` + `PluginSupervisor` + `ActionService`: a real delayed action is
  parked on g1, the sealed g2 replacement is installed/staged and started through
  the supervisor, g1 work is reconciled on g1, the package is rolled back to g1,
  and a further action completes — with the independent plugin-side journal
  compared against the durable receipts.

The shipped entry only attaches the action service on an interactive TTY; the
proof cannot allocate one without a new dependency, so it reaches the same
production functions in-process (documented in the module header). The real
`hub.PublishReport` RPC and the real plugin `plugin.Invoke`/`plugin.Reconcile`
RPCs are exercised end-to-end over sockets.

## Digests (current revision)

| Artifact | sha256 |
|---|---|
| hub binary `target/debug/agalma` (`d62afd4`) | `f8820ee7830882bcf4e3a375c01e3b0414a71e88ec20038321c1bee8f4dc6631` |
| `fixtures/plugin-pack/src/main.rs` | `529c09e8666339ec979b441ae8e6e2e4805b9012972b9a22bcf98356315df9dd` |
| `fixtures/plugin-pack/manifest.json` | `046320923f98ff0241ddbd9f43a3cfa735b692924e40f6c28bd66d2e1d1400c6` |
| `fixtures/plugin-pack/config-schema.json` | `e4c44d80c21341ac3503ba7c6558d4bf8820bf9569565c0977218f37f6c9965a` |
| `fixtures/plugin-pack/default-config.json` | `d645c08784c740b3b612109997cfe6d2abe7e3cba4fc9b0a6f05340360d17b8a` |
| `fixtures/plugin-pack/report-schema.json` | `1c5c73d21fd0e0a4910c73c3118e9c099a0d5eb112c71e2a98d84a478352f5b2` |
| `fixtures/plugin-pack/Cargo.toml` | `478596957511cc04d17b55c31bd69f552d90243b4268e4dae0e771c9f1616873` |
| `fixtures/plugin-pack/Cargo.lock` | `f759953e96473bc66b4eec7028eafabd53f094ad7c0e8e7c9c7370d906ea35a8` |
| `fixtures/plugin-pack/README.md` | `465a7f7114d4d339717ffc10afcbca89e78d25c10ea8bb63b178de50323b0bf5` |
| built `target/plugin-pack-build/debug/plugin-pack` | `0f29872e329989da995514c335a9e369be8ee63a27ee4b008a9fd743ed760c17` |

### Historical digests (preserved; earlier revisions)

| Artifact / revision | sha256 |
|---|---|
| hub binary `6b685e0` | `33fe508050124cb141362149ebaa8b77478ac352619419e480de9d3144f2c970` |
| hub binary `3eabeed` | `bdca094d7d12b9ceeb27ebfbe8241633e415b632fe9011dde732aa298ca8a0fd` |
| hub binary `27056f1` | `ecc3ff7570e597f676183710f48c02f320132080fbc7bd904b18d8da6d9a424d` |
| `fixtures/plugin-pack/src/main.rs` (pre-revision) | `8b829c3756e36e2dab7ca4d3e28cb607da850017778ebb82a186bb51d378e962` |
| `fixtures/plugin-pack/README.md` (pre-revision) | `08087eb8fcd5d1c2d84db6bb331505926de6014e2d5956df12e4cf667ce48f01` |
| built `plugin-pack` (pre-revision) | `e19f6133c176685b96f6f04cc05df3afd4b95394f3dfcef9b101ac4c52d62c84` |

The hub binary digest is pinned before the fixture is built and re-asserted in
every action/restart/replacement/rollback case; the fixture generations never
rebuild the hub.

## Fixture build (documented offline command)

```
cargo build --offline --manifest-path fixtures/plugin-pack/Cargo.toml --target-dir target/plugin-pack-build
```

`fixtures/plugin-pack` is its own Cargo workspace (`[workspace]` in its
`Cargo.toml`); the product workspace does not build it. It uses only public
contracts (`agalma-contracts`) and the common transport (`agalma-ipc`), plus its
already-pinned deps; no new downloads. The fixture now announces the real
installed `package_version` (a `0.2.0` replacement announces `0.2.0`) and
supports the delayed-action channel used by the upgrade proof.

## Focused proof (required entry point)

```
CARGO_TARGET_DIR=/Users/connorfranc/code/agalma/target/worktree-caches/agalma-jmr.8/target \
BEAD=agalma-jmr.8 make test-focused SELECTION='-p agalma-conductor --test hp_plugin_proof'
```

Result (integrated `main` `d62afd4` + repaired proof): **passed — 13 passed, 0
failed, 0 ignored**. Evidence:
`target/test-runs/agalma-jmr.8/20261018T235430Z-19244/` (`summary.json`).

Architecture checks (`-p agalma-conductor --test architecture`): **passed — 9
passed, 0 failed, 0 ignored**. Evidence:
`target/test-runs/agalma-jmr.8/20261018T235441Z-20351/` (`summary.json`).

### Focused run history (preserved)

| Run id | Revision | Result |
|---|---|---|
| `20261018T235430Z-19244` | `d62afd4` + repaired proof (exact package version) | 13 passed / 0 failed |
| `20261018T084041Z-58977` | `d62afd4` + repaired proof (case-9 teardown) | 13 passed / 0 failed |
| `20261018T050612Z-97602` | `d62afd4` + repaired proof | 13 passed / 0 failed |
| `20261018T044849Z-88455` | `d62afd4` + repaired proof | 13 passed / 0 failed |
| `20261018T044417Z-69329` | `d62afd4` + updated proof | 13 passed / 0 failed |
| `20261018T044246Z-58401` | `d62afd4` + updated proof | 13 passed / 0 failed |
| `20261018T043328Z-88496` | `6b685e0` + revised proof | 13 passed / 0 failed |
| `20261018T040313Z-9994` | `6b685e0` | 11 passed / 0 failed |
| `20261018T025531Z-16934` | `3eabeed` | 10 passed / 0 failed |
| `20261018T025443Z-13904` | `3eabeed` (intermediate) | 9 passed / 1 failed |
| `20261018T023154Z-21251` | `27056f1` (pre-repair) | 9 passed / 1 failed — reproduced the generation-reuse defect |
| `20261018T022951Z-8468` | `27056f1` | 9 passed / 0 failed (before case 8) |

### Cases and outcomes (current tests)

| Case | Outcome |
|---|---|
| Unknown package installs; data visible | PASS — `agalma plugins --once` reports `roots=1 installed=1`, exit 0; a real `hub.PublishReport` renders the declared view with the plugin payload in the production read model |
| Health (live read model) | PASS — the production host identity-checks the typed `plugin.Health` reply against its pinned binding and records it; `PluginHost::plugin_components()` exposes `PluginComponent.health = { state: ready, detail: "plugin-pack serving…" }` for the exact generation, and the same panel is overlaid onto the shipped watch UI each tick (HP.6) |
| Supervisor-observed crash/recovery (case 4b) | PASS — the child is `kill -9`'d and **only** `PluginHost::supervise_tick` is driven: `outcome.crashed` observes the exit, and `PluginHost::plugin_components()` reports the pinned generation `lifecycle: failed` / `disconnected: true` with the supervisor's `child exited (crash #1)` reason — no `record_health`/`confirm_ready_health` injection. The shipped watch projection is asserted too: `App::overlay_plugins(host.plugin_components())` rendered through `render_to_string` shows `example.plugin-pack … failed … DISCONNECTED` and the crash reason; the next due tick restarts the child, the identity-checked health gate restores `ready / connected`, and the rendered frame shows `health: ready` with no `DISCONNECTED` |
| Permitted action runs once | PASS — `flush` (grant `ledger.read`) runs once via the UI key path; one independent effect-journal line; durable terminal receipt |
| Denied action zero effects | PASS — `danger` (requires `ledger.write`, ungranted) is refused; zero effect-journal lines; no refresh |
| Child restart preserves identity/receipts | PASS — killed child restarts under the production supervisor under backoff; plugin id/generation (1) preserved; the pre-restart receipt stays durable; a second dispatch runs once under a distinct operation id |
| Upgrade: drain/stage/reconcile and unresolved-work guard (case 5) | PASS — a real delayed `flush` is parked unresolved on g1 and pinned in the registry ledger; `begin_drain(g1)` marks the active g1 generation `Draining`; installing/staging g2 leaves g1 active and g2 pending (staging never switches the active generation). While the g1 pin is unresolved, `remove(g1)` is refused with `EffectUnconfirmed`; after reconciling g1 (one terminal receipt, no repeated effect) and resolving the pin, the only remaining refusal is `RevisionConflict` (g1 still active), proving the earlier refusal was specifically the unresolved pin. Only then does the supervisor health-gate g2 to active; the g1 receipt survives and no effect is replayed |
| Rollback: restore g1 + a further action | PASS (production sequence) — g1 is re-staged and restarted through the supervisor; the restored package digest equals the previous sealed package; a further real `flush` completes immediately as a distinct durable effect pinned to g1 |
| Hub reassembly preserves action binding/receipt (case 9) | PASS — the first host is **fully torn down** before reassembly: its runtime (and the hub capability serve task), owned child, host, health probe, action service, action driver and selected `LedgerHandle` are all shut down and dropped, and only the expected durable record identity (`pin_a`/`op`/`rec_a`) plus the on-disk state cross the boundary. A fresh runtime/host/ledger is assembled through the production entry points; the post-restart service is queried over the reassembled host's own `ledger_b`, and the full durable `ActionRecord` (generation, deadline, intent identity, terminal state, receipt content) is exactly equal to the pre-restart record. The exact `BindingPin` survives, reconciling the completed operation re-runs no effect, and the independent journal effect count stays 1 |
| Hub restart with a replacement (generations, case 8) | PASS — the shipped `agalma plugins --once` runs twice over one selected ledger with a replaced package: the replacement allocates a new monotonic generation 2 (`max=Some(2)`), generation 1 keeps the previous package (no redirect), and the previous generation stays active while the replacement is a pending candidate; each stored generation reports its exact package version read from its installed manifest (g1 `0.1.0`, g2 `0.2.0`); the hub binary digest is re-asserted unchanged |
| Missing capability | PASS — `auto_approved_capabilities` is exactly `requested ∩ {report.publish, ledger.read}`; `ledger.write`/unknowns never granted |
| Long state path | PASS — a >100-byte state dir uses the SUN_LEN-safe socket fallback; the plugin still launches and reports |
| Stale report | PASS — a report observed past the stale threshold (default 15 s) marks the plugin `stale` in the read-model derivation |
| Exact replacement package version | PASS — the g2 `BindingPin.implementation_version` is compared to the replacement's own `package_version` (`manifest2.package_version`) and separately to `0.2.0`; case 8 reads g1/g2 versions from their installed manifests. No hardcoded default version is trusted |
| Hub binary digest unchanged | PASS — pinned before the fixture build and re-asserted in every action/restart/replacement/rollback case, in the generated action/restart property (case 7), and in the CLI replacement reassembly (case 8) |

Deterministic property coverage:

* Action/restart oracle (case 7): generated `flush`/`danger`/restart sequences,
  fixed seed `0x0bd3_5eed_0000_00a8`, budget 16 cases; oracle = exactly one
  durable effect per allowed dispatch, zero for a denied dispatch, distinct
  operation identity per dispatch.
* Delayed reconcile (case 10): generated delayed dispatches, fixed seed
  `0x0bd3_5eed_0000_00a9`, budget 12 cases; oracle = one effect per delayed
  dispatch, unresolved until reconcile, settled exactly once on the pinned
  generation, no repeated effect.

## Full workspace acceptance

The required full workspace acceptance (`make test-full`, unified runner,
`cargo test --workspace`) has been taken on this integrated revision
(`d62afd4`). Command:

```
CARGO_TARGET_DIR=/Users/connorfranc/code/agalma/target/worktree-caches/agalma-jmr.8/target \
AGALMA_TEST_RUN_ROOT=/Users/connorfranc/code/agalma/target/test-runs \
BEAD=agalma-jmr.8 make test-full
```

| Run id | Revision | Result |
|---|---|---|
| `20261018T235548Z-28425` | `d62afd4` (repaired proof) | **PASS** — `exit=0`, `duration_ms=1137995`; `tests/architecture.rs` 9/9, `tests/execution_process.rs`, and the 13-case `tests/hp_plugin_proof.rs` all pass |
| `20261018T235447Z-20739` | `d62afd4` (first attempt) | FAILED — environmental `SUN_LEN`; worktree `target/test-runs` path pushes the ledger socket to 118 bytes > 104. Retained unchanged |
| `20261018T040335Z-12166` | `6b685e0` (pre-revision proof, 11 cases) | PASS — earlier scope; superseded |
| `20261018T025848Z-32332` | `3eabeed` (10 cases) | PASS — earlier scope; superseded |
| `20261018T025629Z-20470` | `3eabeed` | FAILED — environmental `SUN_LEN`; retained |

Note (environmental, preserved): running `make test-full` with
`AGALMA_TEST_RUN_ROOT` under the worktree (or the per-bead subdirectory) pushes
the fixed `<root>/ep-<pid>-<nanos>-<seq>/a/ledger.sock` path over the macOS
`SUN_LEN` limit (104; the worktree run measured 118 bytes). Use the primary
`AGALMA_TEST_RUN_ROOT=/Users/connorfranc/code/agalma/target/test-runs` (90 bytes).
The runner's own evidence still lands under `target/test-runs/agalma-jmr.8/<run-id>/`.
The `SUN_LEN` failure is environmental, not a product regression.

## Production-path limits (concrete, reproducible; HP.8 owns no production file)

1. **The private launch channel does not convey the spoke's pinned binding.**
   `PluginHost::launch_request` sets `AGALMA_HUB_SOCKET`,
   `AGALMA_PLUGIN_GRANT`, `AGALMA_PLUGIN_GRANT_CAPABILITY` and
   `AGALMA_PLUGIN_REPORT_GRANT`, but not a `BindingPin/v1`. A
   `hub.PublishReport` publication must carry the exact authenticated binding
   (`plugin_router::publish_report` compares `publication.binding` to the grant
   scope), so a spoke cannot report without it. Workaround owned by HP.8: the
   fixture derives its own binding from its hub-owned installed location
   (`g<generation>` + the sealed manifest's default-configuration digest), which
   the auto-install pin makes exactly equal to the host binding. Owner of the
   real fix: HP.3/HP.4 (convey the binding on the private launch channel).
2. **Configured-root auto-install installs but does not start.** `assemble_inner`
   leaves a freshly auto-installed plugin's desired state at `Stopped`, so
   `agalma plugins --once` installs (`installed=1`) but spawns nothing; a launch
   needs an explicit `set_desired(Running)` (production API). The proof calls it.
3. **Health is a live overlay, not durable (HP.6 landed `d62afd4`).** The
   transient `plugin.Health` observation is now overlaid onto the shipped watch
   UI every tick by `run_interactive_with` through
   `PluginHost::plugin_components()`, so the shipped UI reflects a crash or
   recovery as soon as the host observes it. It remains non-durable: the
   *stateless* `status`/`watch` snapshot rehydrates from the selected ledger and
   cannot reconstruct a transient health observation, so a **hostless** snapshot
   shows the durable lifecycle only (see the note below).
4. **Generations are restored across hub assembly (HP.5 `3eabeed`).** The
   production host restores installed generations from the selected-ledger
   `RegistryCheckpoint` plus the hub-owned immutable install store before
   discovery allocates: a restart reuses the prior generation for the same
   package and a replacement allocates a strictly higher, never-reused
   generation. Case 8 asserts this over the shipped `agalma plugins --once`
   binary. The pre-repair failure is retained under run `20261018T023154Z-21251`.
5. **A draining generation is not served concurrently.** The production
   supervisor owns one child per plugin, so pinned work on the prior generation is
   reconciled while that generation is still the live launch (case 5), not while
   the replacement runs. Concurrent drain/reconcile for a replaced generation
   remains a platform capability gap (owner HP.3/HP.5).
6. **The action service and the registry pin ledger are not wired together.** The
   registry refuses removal of a generation with unresolved pins
   (`EffectUnconfirmed`) and refuses to serve it once drained — but the HP.7
   `ActionService` journals its unresolved operations in its own
   `component-store/v1` journal, and the production host does not mirror a
   still-unresolved journaled operation into `RegistryEntry::pins`. Case 5
   therefore pins the unresolved `op1` explicitly through the production
   `PluginRegistry::pin_operation` API to exercise the removal guard
   (`remove` → `EffectUnconfirmed`), then resolves it after reconcile. The real
   fix (mirror a journaled, unresolved action into the registry pin ledger at
   dispatch) belongs to the HP.3/HP.7 owner.

The selected read model also labels discovered plugins `lifecycle=installed`
(the durable registry checkpoint does not carry the observed lifecycle), so a
**hostless** `status`/`watch` snapshot shows `installed` rather than `ready`. The
HP.6 live overlay (`PluginHost::plugin_components()`) supersedes this whenever a
host is attached, so the shipped UI shows the live `ready`/`failed`/`disconnected`
state.

## Next decision

Focused repaired proof 13/13 (`target/test-runs/agalma-jmr.8/20261018T235430Z-19244/`),
architecture 9/9 (`…235441Z-20351/`), and the required full workspace acceptance
PASS (`…235548Z-28425/`, `exit=0`) on integrated `main` `d62afd4`. The evidence
is complete; the preserved `SUN_LEN` failure is environmental. Remaining:
independent review, then commit and close (not done by this session).

No commit, merge, close or removal is made from this session. No production file
is owned or changed by HP.8.
