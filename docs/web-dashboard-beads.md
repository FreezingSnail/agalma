# Browser dashboard bead index

Created 2026-10-13 by Sol (`gpt-6`). Epic: `agalma-rwn`.
Canonical accepted design: [web-dashboard-design.md](web-dashboard-design.md).
The user approved local, read-only browser viewing and requested decomposition.

Eight children are `decomp-approved`, `staged`, `difficulty:high`, and deferred
to 2099-01-01. Planning validation does not grant implementation/spending release.
Implementation belongs to GPT-6 Luna or DeepSeek Flash through OpenCode; no
implementation session was dispatched for these beads.

## Tasks and direct blockers

| Bead | Owned outcome | Blocking prerequisites |
|---|---|---|
| `agalma-rwn.1` | typed control-plane read contracts and browser wire fixtures | `agalma-jmr.1` |
| `agalma-rwn.2` | hub-owned read projections and scoped capability routing | `agalma-rwn.1`, `agalma-jmr.4`, `agalma-jmr.5`, `agalma-jmr.6` |
| `agalma-rwn.3` | bounded loopback HTTP read transport in IPC | `agalma-rwn.1`, `agalma-jmr.4` |
| `agalma-rwn.4` | independently installable dashboard package and supervised listener | `agalma-rwn.2`, `agalma-rwn.3`, `agalma-jmr.3` |
| `agalma-rwn.5` | generic plugin view blocks and required browser fixture harness | `agalma-rwn.4` |
| `agalma-rwn.6` | factory pages and registry-derived plugin navigation | `agalma-rwn.5` |
| `agalma-rwn.7` | snapshot polling, freshness and hub restart recovery | `agalma-rwn.6` |
| `agalma-rwn.8` | unknown-plugin browser install and restart acceptance | `agalma-rwn.7`, `agalma-jmr.8` |

The epic blocks on `agalma-jmr` (HP). No HP or CV task depends on this epic.
HP.8 proves the platform; WEB.8 adds browser acceptance after that evidence.
CV.2's standalone CLI migration remains its own work. WEB.2 adds the shared
hub read surface without reusing the current direct-SQLite status path.

Every child has complete scope, owned files, typed contracts, observable
acceptance, intake block, structural difficulty, notes and stopping condition.
The MACHINE symbols describe sibling interfaces because the current validator
resolves those within an epic. Actual bd edges enforce external HP prerequisites.
Canonical contracts/ownership details are in the design; implementers read their
complete assigned bead locally, without expanded parent/dependency bodies.

## Focused acceptance commands

These commands are future implementation acceptance, not checks run in planning.
They use the workflow's existing focused entry point while the unified runner
is pending. Required real server/browser prerequisites must be stated separately.

| Bead | Command |
|---|---|
| `agalma-rwn.1` | `cargo test --offline -p agalma-contracts --test control_plane_protocol` |
| `agalma-rwn.2` | `cargo test --offline -p agalma-conductor --test control_plane_read` |
| `agalma-rwn.3` | `cargo test --offline -p agalma-ipc --test web_http` |
| `agalma-rwn.4` | `cargo test --offline -p agalma-web-dashboard --test package_lifecycle` |
| `agalma-rwn.5` | `cargo test --offline -p agalma-web-dashboard --test view_blocks` |
| `agalma-rwn.6` | `cargo test --offline -p agalma-web-dashboard --test dashboard_pages` |
| `agalma-rwn.7` | `cargo test --offline -p agalma-web-dashboard --test refresh_recovery` |
| `agalma-rwn.8` | `cargo test --offline -p agalma-web-dashboard --test browser_plugin_proof` |

WEB.5 introduces reusable browser test support using installed `CHROME_BIN`.
A missing browser or required binary, skipped required case, or zero-case
selection is a failure. Browser fixtures prove rendering; WEB.8 must separately
prove production discovery/report routing/restart with unchanged hub/dashboard
binary SHA-256 digests and an independently built unknown reporting plugin.
The fixture build uses `target/web-dashboard-plugin-build` as a stable isolated
cache. No browser download or live-provider check is required.

WEB.8 runs one full workspace acceptance after its focused proof:
`cargo build --offline --workspace --bins`, then
`cargo test --offline --workspace`. Preserve established environment and
serialization requirements and state the broader cost before execution.

## Ownership and handoff

WEB.2 waits for HP.4/HP.6 before shared conductor edits. WEB.3 waits for HP.4
before IPC exports. WEB.4 owns root Cargo and package/build wiring. Its std-only
build logic creates a compiled inventory from committed assets, so later browser
modules need no server source edit. WEB.5 owns views; WEB.6 owns the shell;
WEB.7 receives app.js integration ownership. Browser test support transfers
WEB.5 -> WEB.6 -> WEB.7 -> WEB.8. Shared edits are limited to those named seams.

The reporting fixture belongs to WEB.8 under fixtures/web-dashboard-plugin;
it does not overlap HP.8's fixtures/plugin-pack. Runtime predecessor defects
return to their owner. Do not weaken accepted contracts or acquire another task's
files during repair. Stop after focused acceptance, or one reproducible blocker
and at most 30 minutes of diagnosis. Save raw logs and status under
`target/test-runs/<bead-id>/`, and return one concise handoff.

## Planning validation

- `target/debug/agalma plan-check --epic agalma-rwn --repo /Users/connorfranc/code/agalma`
  returned `OK`, exit 0, for all eight staged children with default count/width
  rules. Raw output: `target/test-runs/web-dashboard-planning/plan-check.log`.
- `bd dep add --actor gpt-6 --file target/test-runs/web-dashboard-planning/dependencies.jsonl`
  added 16 blocking edges, exit 0. Parent-child edges came from child creation.
- `bd dep cycles` reported no dependency cycles, exit 0. Raw output:
  `target/test-runs/web-dashboard-planning/cycles.log`.
- Sol marked the clean direct-authored decomposition `decomp-approved` after
  validation. This is not designer-session, runtime sweep, browser, or factory
  execution evidence. All eight tasks remain deferred.

The next implementation prerequisite is HP.1 for WEB.1; hub projections require
HP.4/5/6. Existing platform release gates remain intact. This planning operation
adds no automatic task release, claims, paid inference, commit or push.
