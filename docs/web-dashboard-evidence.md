# WEB.13 focused acceptance evidence (`agalma-rwn.13`)

Implementation model: `opencode-go/deepseek-v4.1-flash`. Reviewer: Sol
(production authority/runtime wiring). No commit/merge/close performed.

## Source identity

- worktree: `/Users/connorfranc/code/agalma/target/worktrees/agalma-rwn.13`
- branch: `task/agalma-rwn.13`
- base (dependency-complete): `190fc14d1ff65ad3081ea9f422bf1e13895c64f3`
- redirected Cargo cache: `target/worktree-caches/agalma-rwn.13/target`
- persistent evidence root: `/Users/connorfranc/code/agalma/target/test-runs/agalma-rwn.13/`

Changed/added files (WEB.13 exclusive ownership):

- `crates/agalma-conductor/src/work/factory_runtime.rs` — additive
  `HostRegistration.state_dir: &Path`, populated unchanged from
  `HostSupervision.state_dir` (Sol-approved minimal handoff; nothing else added).
- `crates/agalma-conductor/src/composition.rs` — production registrar binds the
  WEB.11 `FactoryControlService` (runtime source + selected ledger + stop +
  exclusive state-scoped `factory-control.lock` owner + live approved-control
  resolver) to the served router; `launch_request` issues distinct per-capability
  grants plus the private `PluginCapabilityBindings/v1` descriptor; new public
  accessors `control_binding_for` / `capability_grant`.
- `crates/agalma-web-dashboard/src/server.rs`,
  `crates/agalma-web-dashboard/src/plugin.rs`,
  `crates/agalma-web-dashboard/src/lib.rs` — parse the private capability
  descriptor (WEB.12 `parse_capability_bindings`), announce the granted
  capability set, mount `GET`/`POST` factory routes only when an actual explicit
  `factory.read/v1` binding exists (read-only default), revoke on shutdown.
- `crates/agalma-web-dashboard/package/plugin-manifest.json` — requests
  `factory.read/v1`, `factory.control/v1` (never auto-granted).
- `crates/agalma-conductor/tests/cockpit_runtime.rs` (new).
- `crates/agalma-web-dashboard/tests/cockpit_queue.rs` (new).
- `docs/web-dashboard.md` — M2 command/URL/control/pause-shutdown/restart/limits.

## Required focused acceptance (the bead's two targets)

Prerequisites: real conductor/ledger/taskqueue/dashboard binaries; installed
`CHROME_BIN`; `jq`; `rg`; `THREADS=1`; isolated ports/browser profile per run.

```sh
CHROME_BIN='<installed chrome>' THREADS=1 \
  make test-focused SELECTION='-p agalma-conductor --test cockpit_runtime'
CHROME_BIN='<installed chrome>' THREADS=1 \
  make test-focused SELECTION='-p agalma-web-dashboard --test cockpit_queue'
```

| Target | Run dir | Result |
|---|---|---|
| `agalma-conductor --test cockpit_runtime` | `rwn13-final1-20261018T220432Z` | status=passed, 9 passed / 0 failed / 0 ignored |
| `agalma-web-dashboard --test cockpit_queue` | `rwn13-final2-20261018T220506Z` | status=passed, 1 passed / 0 failed / 0 ignored |

(Earlier passing run dirs `rwn13-focused-conductor-20261018T213402Z` and
`rwn13-focused-dashboard-20261018T213438Z` are retained; the `rwn13-final*`
runs are authoritative after a forced rebuild — see the cache note below.)

`cockpit_runtime` (9 cases):

- registrar receives `HostSupervision`'s configured `state_dir` unchanged and
  installs `factory.read/v1` + `factory.control/v1` on the served router;
- generic multigrant descriptor (`PluginCapabilityBindings/v1`) with distinct
  per-capability grants, legacy first-grant env preserved;
- configured-root auto-install stays read-only (no descriptor, no control
  binding; `factory.control/v1` absent from `SAFE_CAPABILITY_ALLOWLIST`);
- control binding rotates with the launch epoch at the pinned generation;
- served router control receipt + exact replay + conflict refusal + revocation
  (zero effects after revocation) with the trusted permission composer observable
  through `factory.read/v1`;
- exclusive control owner refuses a second owner;
- factory stop-exception scoped to the declared factory capabilities;
- 256 deterministic property sequences (seed `0x57454213`, ≤32 events) over
  observation/control/stop/authority/conflict/restart events asserting no read
  effect, one claimant, no unauthorized action, exact replay/unknown, frozen
  recovery, and unavailable-never-healthy;
- frozen recovery replays the durable receipt exactly across a restart.

`cockpit_queue` (1 production case):

- real isolated ledger + real `PluginHost::assemble_with_ledger`, real sealed
  `agalma-web-dashboard` launched with the composition-owned registrar;
- real overview read and Idle factory snapshot with `permissions.pause=true`;
- observation-only GET/reload creates no control intent;
- operator session/CSRF bootstrap; foreign-Origin and missing-CSRF POSTs denied
  with zero effects; explicit pause → applied durable receipt; exact replay →
  frozen receipt; lookup without applying; Resume without phrase → denied;
  confirmed Resume → applied (latch cleared);
- an established `STOP` **file** is removed by a confirmed Resume;
- an unremovable `STOP` (a directory) yields `EffectUnconfirmed` (HTTP 202), the
  sentinel is preserved, and authoritative lookup reports unconfirmed — a
  fail-closed outcome, never a claimed success;
- the installed Chrome renders the real page shell.

## Affected-consumer run (same session)

```sh
CHROME_BIN='<installed chrome>' THREADS=1 make test-focused SELECTION='-p agalma-conductor --test hp_routes -p agalma-conductor --test hp_actions -p agalma-conductor --test hp_registry -p agalma-conductor --test factory_control -p agalma-conductor --test control_plane_read -p agalma-ipc --test factory_http -p agalma-ipc --test web_http -p agalma-contracts --test factory_contracts -p agalma-web-dashboard --test package_lifecycle -p agalma-web-dashboard --test cockpit_page -p agalma-web-dashboard --test first_launch'
```

Run dir `rwn13-affected-consumers-final-20261018T220515Z`: status=passed,
263 passed / 0 failed / 0 ignored.

A mid-session baseline check (detached worktree at `190fc14`, removed after)
shared this redirected Cargo cache and left base-built `agalma-conductor`
artifacts whose mtimes were newer than this worktree's sources. One focused run
(`rwn13-final--p_*`, retained) then linked the stale base artifacts and failed to
compile against the WEB.13 API. The worktree sources were re-touched to force a
correct rebuild, and every result above is from after that rebuild. The failed
`rwn13-final--p_*` run dirs are preserved but are not valid evidence.

## Known pre-existing failures (not caused by WEB.13)

All three reproduce on clean base `190fc14` (verified by running the single test
in a detached baseline worktree at `190fc14` sharing the redirected cache):

- `-p agalma-conductor --test factory_runtime` →
  `watch_signal_drains_at_a_cooperative_boundary_during_active_execution` panics
  at `factory_runtime.rs:2511` ("the task reached active build execution");
  confirmed failing identically on base. The watch runtime itself starts, runs
  reconcile, serves the hub and drains on SIGINT (manual smoke).
- `-p agalma-conductor --test hp_reports` →
  `assembled_host_over_process_ledger_routes_publish_report`,
  `migration_and_complete_state_run_on_both_ledger_transports` fail on the
  missing supervised ledger **server binary** (process transport).
- `-p agalma-conductor --test hp_ui` →
  `process_transport_reads_the_same_production_read_model` fails the same way.

## Remaining gaps / notes

- `cockpit_queue` uses a deterministic reference observation cell (Idle) rather
  than driving the full `agalma work --watch` worker loop; the continuous
  runtime/lifecycle itself is WEB.10's focused acceptance
  (`agalma-conductor --test factory_runtime`), and the browser scenario here
  proves the WEB.13 control/read join over the real ledger, hub and dashboard.
- The full M2 narrative bullets about long-running phase gates and multi-restart
  recovery are exercised at the source level in `cockpit_runtime` (property and
  frozen-recovery cases) rather than re-driven end to end in the browser test.
- No commit/merge/close/worktree-removal/push performed; diff returned for Sol.

## Repair after Sol REVIEW: DRIFT (2026-10-18)

Implementation model: `opencode-go/deepseek-v4.1-flash`. No commit/merge/close.

Findings repaired (same worktree/branch/base `190fc14`, redirected cache):

1. **Production queue acceptance.** `cockpit_queue` now starts the real
   `agalma work --watch` continuous runtime (deterministic local task harness:
   `AGALMA_ACTIVITY_MODE=scripted` + git/bd workspace + frozen
   `protected-inputs.json`), the runtime discovers/installs/launches the sealed
   `agalma-web-dashboard` package (explicit operator approval), the installed
   browser opens the runtime-printed URL, and the test proves empty→`Idle`→one
   approved eligible task automatically claimed and executed→busy page/snapshot
   responsive→pause prevents a later claim→confirmed Resume allows exactly one
   later claim. No manual observation adapter is used for queue state.
   Enabling change: `composition::assemble_work_plugin_host` reads the
   **existing** durable hub installation pin (`HubInstallStore`, the persisted
   `PluginInstallation/v1` `installation.json` per installed generation) for each
   discovered package, accepted only when its frozen package digest matches the
   discovered bytes, and requests a matching pinned package `Running`. No new
   persistence, no env map, no id-only allowlist.
2. **Grant lifecycle.** `PluginHost::launch_request` no longer clears the
   state-wide control binding on every launch; the binding map is keyed by exact
   plugin id (`clear_control_binding_for`) so launching an unrelated plugin
   cannot disable the dashboard's controls. Regression:
   `unrelated_launch_preserves_the_dashboard_control_binding`.
3. **Owner lock race.** `FactoryControlOwner::acquire` is now atomic: the owner
   pid is written to a private temp file and the lock path is published with a
   single `hard_link`, so a lock never exists without its pid; recovery unlinks
   only a well-formed **dead** pid and never an empty/malformed lock. Regression:
   `concurrent_control_owner_acquisition_admits_at_most_one_owner` (64 rounds × 8
   racing acquirers → exactly one owner).

Prerequisite for `cockpit_queue`: the real product binaries must be built
(`cargo build --offline --locked --workspace --bins`), because the runtime
spawns the ledger/taskqueue/workspace/harness servers.

### Repaired focused acceptance

| Target | Run dir | Result |
|---|---|---|
| `agalma-conductor --test cockpit_runtime` | `rwn13-repair1-20261018T224854Z` | passed, 11/0/0 |
| `agalma-web-dashboard --test cockpit_queue` | `rwn13-repair2-20261018T224927Z` | passed, 1/0/0 |

### Repaired affected consumers

`rwn13-repair-affected-20261018T225052Z`: `factory_runtime`, `factory_control`,
`hp_routes`, `hp_reports`, `hp_actions`, `hp_registry`, `hp_ui`,
`control_plane_read`, `factory_http`, `web_http`, `factory_contracts`,
`package_lifecycle`, `cockpit_page`, `first_launch` → status=passed,
349 passed / 0 failed / 0 ignored.

The earlier "known pre-existing" `factory_runtime`/`hp_reports`/`hp_ui` failures
were a missing supervised **server binary** in the focused cache; building the
workspace bins resolves them (they are a real prerequisite, now declared).

Prior green runs remain preserved under the same evidence root but are not final
evidence for the repaired source.

## Repair after Sol re-review (current-pin authority) — 2026-10-18

Implementation model: `opencode-go/deepseek-v4.1-flash`. No commit/merge/close.

- `operator_installations` now consumes only the plugin's **current (newest
  installed) generation** pin (`HubInstallStore::installed_generations` max +
  `load_installation`); there is no ascending scan and no fallback to an older
  granting generation, so a newer generation whose pin drops `factory.control/v1`
  revokes the older grant.
- A pin is treated as an explicit operator grant only when it approves a
  capability outside `SAFE_CAPABILITY_ALLOWLIST`; the automatic auto-install pin
  (exactly the safe intersection) is never an approval, and configured-root
  auto-install stays read-safe.
- Regressions added: `newer_generation_revokes_older_factory_control_grant`,
  `unrelated_plugin_safe_pin_never_gets_control` (plus the existing
  `changed_package_digest_*`, `unrelated_same_id_package_*`).

### Fresh evidence

| Target | Run dir | Result |
|---|---|---|
| `agalma-conductor --test cockpit_runtime` | `rwn13-drift3-conductor-20261018T231102Z` | passed, 18/0/0 |
| `agalma-web-dashboard --test cockpit_queue` | `rwn13-drift3-queue-20261018T230940Z` | passed, 1/0/0 |
| affected consumers | `rwn13-drift3-affected-20261018T231141Z` | passed, 349/0/0 |

### Residual contract gap (for Sol)

A strict rule "only the one dashboard plugin may hold control, even if a
*different* plugin id has an operator pin that approves `factory.control/v1`"
cannot be derived from existing base APIs. Nothing on base names the
control-surface/dashboard plugin: `HubInstallStore` pins, `RegistryCheckpoint`
(desired/active/max generations) and `LedgerDesiredStateStore` carry no such
identity, and a plugin id constant in conductor source is explicitly forbidden.
Implementing that strict rule needs either (a) an explicit configured
control-surface identity contract, or (b) a new persisted identity — both
outside the current authority. The current implementation therefore grants
control to any plugin whose *current, digest-matching* pin is an explicit
operator grant (a capability outside the safe allowlist); a same-id/unrelated
auto-install pin or a changed digest never qualifies.

## Repair after Sol amendment: `--factory-control-plugin` selector — 2026-10-18

Implementation model: `opencode-go/deepseek-v4.1-flash`. No commit/merge/close.

Sol amended the bead to add `crates/agalma-conductor/src/cli.rs` ownership and an
optional `work --watch --factory-control-plugin <plugin_id>` selector, resolving
the control-surface identity gap.

- `cli.rs`: `WorkArgs.factory_control_plugin: Option<String>` (`--factory-control-plugin`).
- `run_watch_with` → `SupervisionHandle::spawn_host_with_factory_control` →
  `HostSupervision.factory_control_plugin` →
  `composition::assemble_work_plugin_host_with_control`.
- `operator_installations` consumes **only** the selected plugin id's current
  (newest installed) durable `HubInstallStore` pin, and only when its frozen
  package digest matches and it explicitly approves `factory.control/v1`. Absent
  selector consumes nothing (read-only); unselected plugins never receive
  control; a newer non-granting generation revokes; absent/changed/mismatched
  pins fail closed. No new persistence, no source plugin-id allowlist, no
  `SAFE_CAPABILITY_ALLOWLIST` change.

Regressions (`cockpit_runtime`): `absent_selector_is_read_only_even_with_control_pin`,
`unselected_plugin_control_pin_receives_no_control`, plus preserved
`durable_operator_pin_binds_control_to_the_exact_package`,
`changed_package_digest_cannot_obtain_control_authority`,
`unrelated_same_id_package_cannot_obtain_control_authority`,
`newer_generation_revokes_older_factory_control_grant`,
`unrelated_plugin_safe_pin_never_gets_control`.

Production (`cockpit_queue`): selected (control) scenario plus
`cockpit_queue_unselected_and_no_selector_stay_read_only`.

### Fresh evidence

| Target | Run dir | Result |
|---|---|---|
| `agalma-conductor --test cockpit_runtime` | `rwn13-selector-conductor-20261018T232300Z` | passed, 20/0/0 |
| `agalma-web-dashboard --test cockpit_queue` | `rwn13-selector-queue-20261018T232335Z` | passed, 2/0/0 |
| affected consumers | `rwn13-selector-affected-20261018T232527Z` | passed, 349/0/0 |

Sibling touch: `crates/agalma-conductor/tests/factory_runtime.rs` `WorkArgs`
literal gained `factory_control_plugin: None` (mechanical compile fix required by
the amended CLI; it is not part of WEB.10's owned surface).

## Final repair (read surface / liveness / no-pin) — 2026-10-18

- **Read surface independent of the selector.** Configured-root auto-install now
  additionally auto-approves the read-only capabilities
  (`control-plane.read/v1`, `factory.read/v1`) via a new
  `AUTO_APPROVED_READ_CAPABILITIES` class list; `SAFE_CAPABILITY_ALLOWLIST` is
  unchanged and `factory.control/v1` is never automatic. The cockpit is launched
  whenever its manifest requests `factory.read/v1` (capability-based).
  `factory.control/v1` is gated per-launch by the new
  `PluginHost.factory_control_plugin`: effective caps strip it unless the plugin
  is the selected id, so no selector / unselected plugins keep reads and receive
  no control token, descriptor entry or permission (durable pins are not
  rewritten).
- **Liveness tri-state.** `PidStatus{Alive,Dead,Unknown}` + public
  `classify_process_probe`; only a confirmed `ESRCH` ("No such process") is
  reclaimable. EPERM, unknown/malformed messages and spawn errors are `Unknown`
  and the lock is preserved.
- **Selected plugin without a pin** fails closed for control while keeping safe
  reads.

New regressions: `unknown_process_status_preserves_the_lock`,
`selected_plugin_without_pin_keeps_reads_but_no_control`; updated
`absent_selector_is_read_only_even_with_control_pin`,
`configured_root_auto_install_is_read_only`,
`unrelated_launch_preserves_the_dashboard_control_binding`. Production
`cockpit_queue_unselected_and_no_selector_stay_read_only` now proves the read
UI/API (`/`, `/api/v1/overview`, `/api/v1/factory`) works with
`permissions.pause=false` and a refused control POST.

### Fresh evidence

| Target | Run dir | Result |
|---|---|---|
| `agalma-conductor --test cockpit_runtime` | `rwn13-final-fix-conductor-20261018T234227Z` | passed, 22/0/0 |
| `agalma-web-dashboard --test cockpit_queue` | `rwn13-final-fix-queue2-20261018T234425Z` | passed, 2/0/0 |
| affected consumers | `rwn13-final-fix-affected-20261018T234707Z` | passed, 349/0/0 |

Retained failed run `rwn13-final-fix-queue-20261018T234310Z` (1/1): the first
task exceeded the old 40s claim bound under the runner; the bound was widened to
90s (timing, not a behavior change) and the rerun passed.
