# Local cockpit bead index

Accepted amendment: 2026-10-18, authored by `gpt-6`. Canonical design:
[web-dashboard-design.md](web-dashboard-design.md). Epic `agalma-rwn` delivers
the first local operator cockpit: real browser page followed by continuous
automatic eligible-queue operation and existing human pause/resume controls.
Implementation remains paused. Design approval, dependency readiness, native
execution approval, successful dispatch and delivered acceptance are distinct.

## First-release tasks

The epic has ten direct children, including the unchanged closed WEB.1. New
contracts create an early handoff; the actual page does not wait for the worker
runtime or deferred rich plugin views. Blocking IDs below omit parent-child and
native approval edges, which are listed separately.

| Bead | Outcome / owned surface | Direct implementation prerequisites | Review |
|---|---|---|---|
| `agalma-rwn.1` | Merged typed summary read/BrowserWire contracts; unchanged | HP.1 (closed) | Already approved |
| `agalma-rwn.2` | Production overview/components; live/source/bound/unknown economics semantics | WEB.1, HP.4/5/6 | Sol |
| `agalma-rwn.3` | Retained GET-only IPC transport; final review pending | WEB.1, HP.4 | Sol |
| `agalma-rwn.4` | Compiling package/executable/config/inventory with independent RPC bootstrap acceptance | WEB.1 via WEB.3, WEB.3, HP.3; no WEB.2 dependency | Separate DeepSeek |
| `agalma-rwn.6` | One page/polling/browser harness and actual production M1 launch | WEB.2, WEB.4, WEB.9; no WEB.5/10 dependency | Separate DeepSeek |
| `agalma-rwn.9` | FactoryRuntime/v1 + FactoryControl/v1 types/trait/golden fixtures | WEB.1 | Sol |
| `agalma-rwn.10` | Continuous queue/idle/paused runtime and independent host/RPC lifecycle | WEB.2, WEB.9 | Sol |
| `agalma-rwn.11` | Injected runtime read adapter and intent/result control replay | WEB.2, WEB.9; no WEB.10 implementation dependency | Sol |
| `agalma-rwn.12` | Separate-grant factory client, bounded POST, local session/CSRF | WEB.3, WEB.9; no WEB.10/11 dependency | Sol |
| `agalma-rwn.13` | Production bindings/registration and actual M2 queue/control browser proof | WEB.4/6/10/11/12 | Sol |

Every amended/new body contains source files, concrete algorithm/contract,
failure/recovery/authority semantics, deterministic property budgets/seeds,
selected checks/prerequisites, ownership transfers and stopping condition. Budgets
in intake blocks are ceilings, not live-provider spending permission. `difficulty`
is the validator's structural tier; `review_route` records the intended reviewer
and routine multi-file UI/package work does not automatically require Sol.

## Parallel ownership

- A: WEB.2 read service/economics/routing, WEB.9 typed contracts, WEB.11 adapters.
  Contract and summary can occupy independent available implementers initially;
  the three-stream limit is an active-work budget, not a static model assignment.
- B: WEB.10 work.rs/CLI/work::factory_runtime and composition after WEB.2.
  Declaring the runtime submodule from work.rs avoids a concurrent lib.rs edit.
- C: WEB.4 Cargo/package/server/build; then WEB.6 assets/page/browser support.
  WEB.4 health.html is separate from WEB.6 index/app. Asset additions need no
  server/inventory edit.
- IPC: finish WEB.3 review, then WEB.12 receives those files and uses a free
  implementation slot. Factory clients use WEB.9 fixtures before live services.
- Join: composition transfers WEB.2→WEB.10→WEB.13; package server WEB.4→WEB.13.
  WEB.11 never owns composition. Browser support begins WEB.6 and is reused by
  later tests rather than replaced.

At most three active implementation sessions and two isolated test binaries by
default. Use named worktrees/branches and absolute redirected cache paths;
coordinate compilation/ports/state/browser profiles. A missing predecessor goes
back to its owner, not an unbounded cross-task repair.
This is a manual dispatch cap. Dependency waves are [1,3,4,1,1], including the
closed contract seed and final joins; queue the fourth ready task until a slot
opens. A stricter median-width-three diagnostic fails and is not reported as a
pass. The normal planner's default width/count/schema checks pass.

## Execution state and gates

The existing WEB.1–WEB.8 execution gates were resolved in earlier user-authorized
work. Keep those historical approvals; unchanged work is not being asked for
approval again. New tasks have native unresolved human gates, each labeled
`human,execution-approval`, metadata `approval_for=<child>`, and one direct blocks
edge. Only explicit human `bd gate resolve` releases them. The current instruction
updates planning; it does not resume implementation or run the factory.

| Task | Gate | Current gate state |
|---|---|---|
| WEB.2 | `agalma-6df` | Closed, prior approval retained |
| WEB.3 | `agalma-3gx` | Closed, prior approval retained |
| WEB.4 | `agalma-5ke` | Closed, prior approval retained |
| WEB.6 | `agalma-4ws` | Closed, prior approval retained |
| WEB.9 | `agalma-avh` | Open |
| WEB.10 | `agalma-6x4` | Open |
| WEB.11 | `agalma-uen` | Open |
| WEB.12 | `agalma-gne` | Open |
| WEB.13 | `agalma-xux` | Open |

WEB.2 remains in_progress with its retained source; WEB.3 is open with an idle
completed implementation attempt, 19/19 latest recorded focused cases, and final
review pending. Neither is accepted/merged. No source/worktree is deleted by
the rescope. New tasks are specified, not dispatched.

## Focused checks and milestones

Use the implemented unified runner via `make test-focused SELECTION=...`, not
the historical direct Cargo commands. Required real binaries are declared with
runner prerequisites. Missing CHROME_BIN/binary, zero cases or skipped required
cases fails. All behavior changes retain independent deterministic properties.

| Task | Required named target(s); affected consumers are detailed in the bead |
|---|---|
| WEB.2 | conductor `control_plane_read`, affected economics/routing |
| WEB.3 | IPC `web_http`, relevant cap/client unit regressions |
| WEB.4 | dashboard `package_lifecycle`; build no-op checks if build.rs changes |
| WEB.6 | dashboard `cockpit_page`, then production `first_launch` |
| WEB.9 | contracts `factory_contracts` |
| WEB.10 | conductor `factory_runtime`, affected protected stop/frozen recovery |
| WEB.11 | conductor `factory_control`, `control::tests`, affected hp_routes |
| WEB.12 | IPC `factory_http`, affected web_http security/drain |
| WEB.13 | conductor `cockpit_runtime`, dashboard `cockpit_queue` |

M1 uses real host/dashboard/ledger and installed Chrome/Chromium, not a fixture
hub. It prints a usable URL and shows actual summary data with optional runtime
unavailable/controls disabled. WEB.4's fixture bootstrap is a package boundary
test, not that end-to-end claim.

M2 uses real continuous runtime/dashboard/browser and deterministic local
reference tasks: empty→Idle→new eligible work→one execution, long busy phase with
responsive reads/health/control, pause holds later claims, confirmed resume,
control replay/conflict/unknown boundaries, stopped-established no-init safety,
frozen restart recovery, one owner and clean shutdown. No paid provider/download.
There is no full suite per task. Later WEB.8 retains its full workspace gate;
broader new runtime checks require a named recorded risk.

Before editing record bead/worktree/branch/base/cache. Evidence root is always
the primary checkout's absolute `target/test-runs/<bead>/<run-id>/`; preserve
all passing/failed/interrupted commands/environment/source/pending identity,
logs/exit codes/fixtures/seeds. Stop after focused acceptance/handoff or one
blocker/30-minute diagnosis. Coordinator commits/merges only reviewed task paths,
stamps closure implementation model, preserves sources/evidence, then guarded
clean worktree/cache removal. No push.

## Deliberately deferred platform work

Epic `agalma-v38` follows the first-release epic and preserves the original
generic-plugin promise. Reparenting keeps original WEB IDs/history; it does not
close tasks or weaken the original proof. These tasks use actual deferred status
with a reason and no sentinel approval date.

| Bead | Later outcome | Direct prerequisites excluding approval |
|---|---|---|
| `agalma-v38.1` | Additive recovered-state provenance design; no code claim | HP.5, WEB.1 |
| `agalma-v38.2` | Versioned report-state/artifact implementation after exact design | v38.1, WEB.2 |
| `agalma-rwn.5` | Six generic browser blocks, consuming existing harness | WEB.4/6, v38.2 |
| `agalma-rwn.7` | Rich navigation and report snapshot/generation/restart recovery | WEB.5/6/13, v38.2 |
| `agalma-rwn.8` | Unknown independently built plugin browser proof and unchanged binaries | WEB.5/7, v38.2, HP.8 |

WEB.5/7 depend on actual report implementation, not design closure. v38.2 remains
explicitly design-required and human-gated until its version/migration/file
details are frozen. v38 is a deferred backlog, not a claim of a validated released
implementation decomposition. WEB.8's focused `browser_plugin_proof` and one
`make test-full` remain protected. HP.8 action/pinning work remains separate.

Deferred `agalma-1o7` designs concierge goal intake after WEB.13. Gate
`agalma-orv` stays open. Planning proposals flow through existing authority into
the same eligible queue; no second scheduler or inferred gate approval.

## Amendment evidence

The prior spec/index remain in Git at the recorded baseline. Before-amendment
own bead fields and the base revision are retained as `beads-before.json` and
`base-revision.txt` under `target/test-runs/web-dashboard-rescope-20261018/`.
Per-bead Markdown drafts and the redundant planning-summary document were
removed after migration at the user's request. Deep implementation details live
in bd; this index is a compact reference, not a second task tracker.
Validation results are recorded after graph/gate/content checks. Historical
planning/runtime receipts and original source/test evidence are not rewritten.

Default `agalma plan-check --epic agalma-rwn` passed (`plan-check-default.log`).
`bd dep cycles` returned no cycles (`cycles.json`). Native gate audit found one
correctly associated direct human execution gate for each of15 audited tasks,
with all8 newly created gates open (`gate-audit.json`). The stronger width-three
diagnostic is retained separately in `plan-check-width3.log`. These are planning
checks, not implementation/build/browser acceptance.

## UI v2 amendment: multi-page cockpit and design pass

Accepted user instruction, 2026-10-18: after M1/M2 delivery the cockpit gains
multi-page navigation, the component registry with health, task/project views
and a visual design pass. New epic `agalma-82f` with four children split by
exclusive file ownership: wave 1 runs three sessions in parallel (the dispatch
cap), wave 2 is the single browser acceptance gate. Data scope unchanged
(existing reads only). Canonical design: [web-dashboard-design.md]
(web-dashboard-design.md) "UI v2 amendment". Planning artifacts:
`target/test-runs/cockpit-ui-v2-plan/`.

| Bead | Outcome / owned surface | Direct prerequisite | Review |
|---|---|---|---|
| `agalma-82f.1` | Client shell, hash router, nav, `ui.js`, six pages, `api.js` validators (all browser JS/HTML) | None (after approval) | DeepSeek |
| `agalma-82f.3` | `dashboard.css` design system pass only | None (after approval) | DeepSeek |
| `agalma-82f.2` | Detail + status routes, pub validator, fixtures, preview server, route tests | None (after approval) | DeepSeek |
| `agalma-82f.4` | Browser suites + `first_launch`; integration gate; gap findings go back to owning beads | `agalma-82f.1`, `agalma-82f.2`, `agalma-82f.3` | DeepSeek |

Execution state: delivered 2026-10-19. All four children were implemented in the
planned waves, reviewed by DeepSeek, repaired where drift was found, committed
and merged: UI.1a `69851bd`, UI.1b `09b53bd`, UI.2a `c098a9c`, UI.1c `3396a6a`,
plus preview repair UI.1d `agalma-82f.5` (`88ca44f`, nested asset-tree copy).
Review findings repaired before merge: status validator types, ReportEnvelope
exact-u64 identities, reduced-motion cascade order. Evidence: wave-1 focused
runs under `target/test-runs/agalma-82f.{1,2,3}/`; integration gate
`target/test-runs/agalma-82f.4/gate3` (18/0: cockpit_page 15, cockpit_queue 2,
first_launch 1); post-merge `package_lifecycle` 9/0
(`target/test-runs/agalma-82f.4/post-merge-package3`). Remaining acceptance:
human visual review of `make start` for the CSS pass, then epic closure.
Runner defect discovered during post-merge evidence (`scripts/test.sh` relative
RUN_ROOT from the primary checkout) is filed separately as `agalma-dns` with an
absolute-RUN_ROOT workaround; it is not part of this increment.

| Task | Required named target(s) |
|---|---|
| UI.1a | none alone (no npm pipeline); verified by UI.1c |
| UI.1b | none alone; visual checklist + UI.1c browser pass |
| UI.2a | dashboard `package_lifecycle` (route cases), ipc `control_plane_client` (pub validator regression), curl preview proof |
| UI.1c | dashboard `cockpit_page`, `cockpit_queue` (must pass unchanged), `first_launch` (navigation-aware) |

Evidence rules are unchanged: focused runner via `make test-focused
SELECTION=...`, installed `CHROME_BIN`, raw logs under
`target/test-runs/<bead>/<run-id>/`, stop at accepted evidence or one
blocker/30-minute bounded diagnosis.
