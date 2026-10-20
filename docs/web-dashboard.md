# Local cockpit and browser dashboard (WEB.6)

This document records the M1 delivery of the local operator cockpit: one page,
browser polling, and the first real production browser launch. It covers the
actual local launch command and URL, the read/status semantics the page renders,
the optional factory endpoint, and the evidence commands. Design rationale lives
in [web-dashboard-design.md](web-dashboard-design.md); the task index is
[web-dashboard-beads.md](web-dashboard-beads.md).

## Scope

M1 proves that one operator can open the printed loopback URL and see real
overview, execution, component-health, economics and timeline data through the
production host. Queue operation, pause/resume effects and restart recovery are
M2 (`agalma-rwn.13`). The cockpit package is independently installed and
supervised; the hub UI imports no plugin code.

Owned files:

- `crates/agalma-web-dashboard/assets/index.html`, `dashboard.css`, `app.js`,
  `api.js`, `refresh.js`, `controls.js`, `views.js`
- `crates/agalma-web-dashboard/assets/pages/` (`component.js` and the other
  route modules)
- `crates/agalma-web-dashboard/tests/browser_support/` (reusable headless
  browser + loopback fixture harness)
- `crates/agalma-web-dashboard/tests/cockpit_page.rs`,
  `crates/agalma-web-dashboard/tests/first_launch.rs`
- `fixtures/cockpit/`
- this document

`api.js` validates response family/version/source metadata and never coerces an
identity, generation, revision or money value through `Number`. `app.js` renders
one page. `refresh.js` owns serial polling. `controls.js` owns explicit pause/
resume. No hub grant or private binding enters browser state.

## Build and local launch

Build all product targets (libraries, server binaries, tests, benches):

```sh
make            # debug: cargo build --offline --locked --workspace --all-targets
make release    # same targets in the release profile
```

The dashboard package is built as the `agalma-web-dashboard` binary with its
sealed manifest under `crates/agalma-web-dashboard/package/`. The production
plugin host discovers configured roots, installs each package, attaches the
selected durable ledger, serves the hub `control-plane.read/v1` router, and
supervises the child:

```sh
# Isolated state directory and a root holding the sealed package.
agalma plugins --state-dir <state-dir> --plugin-root <package-root>
```

On a successful bind and authenticated read the child prints its actual URL to
stderr, and the same URL appears in `plugin.Health` detail as `;url=`:

```text
DASHBOARD_URL http://127.0.0.1:<port>
```

Open that URL in a browser. Port `0` (the default) selects a free port, so the
printed value is authoritative. `http://127.0.0.1:<port>/` serves
`assets/index.html`; `/health.html` remains the package-level health page.

Read routes:

| Route | Behavior |
|---|---|
| `GET /api/v1/overview` | `ControlPlaneRead/v1` overview (stop, executions, economics, timeline, sources) |
| `GET /api/v1/components?limit=&cursor=` | `ControlPlaneRead/v1` component page, default 50 / bound 100, opaque cursor |
| `GET /api/v1/components/{plugin_id}?limit=&cursor=` | `ControlPlaneRead/v1` component detail for a listed or built-in identity; a valid unknown id is `200` with explicit `missing` sources |
| `GET /api/v1/factory` | optional factory runtime snapshot (see below) |

Component detail is delivered through the same production
`control-plane.read/v1` service and router as the list. It reuses the bounded page
contract (default 50, maximum 100) and the existing cursor rules: malformed
cursors are refused and a stale revision is a `409 cursor_conflict` so the page
restarts once. Artifact chunk reads remain explicitly deferred and are not part
of the detail contract. Registration is determined only by the `registry` source:
only its `missing` state renders “not registered”. A known component with
`missing`, `empty`, `unknown` or `unreadable` reports still renders its binding
and declared views with an honest no-report state, and an `unreadable`/`unknown`
registry is never reported as absent.

### Read capability approval

The production auto-install path grants a discovered package exactly its
requested capabilities intersected with the safe allowlist
(`report.publish`, `ledger.read`). `control-plane.read/v1` is **not** in that
allowlist, so an unapproved install yields a bound listener with no read binding:
health `Degraded` and every read route answering `503 read_unavailable` — never a
healthy empty page. The read capability becomes available through the
operator-managed explicit installation pin (the `first_launch` smoke approves it
explicitly) or the later per-launch binding channel owned by WEB.12/WEB.13; it is
never inferred from a package report or a UI actor field. That seam is the
concrete integration point for the control-plane binding work.

## Status semantics

Health distinguishes two independent facts: `listener` (the loopback listener is
bound) and `binding` (the hub read binding is established). `Ready` requires
both; a bound listener without a binding is `Degraded`; a failed listener is
`Unavailable`.

The page distinguishes unknown from healthy/zero/stopped:

- Runtime status is the typed factory status when a factory snapshot is present.
  A missing factory endpoint is rendered as `Unavailable (endpoint absent)`, and
  an absent or failed read source as `Unknown` — never `Idle` or `Running`.
- Unknown costs, allowance and usage are rendered as `unknown`, never numeric
  zero.
- Source states (`available`, `empty`, `missing`, `unreadable`, `unknown`) each
  have their own label; `empty` (no entries) is not `missing`.
- A failed source keeps its last successful data visible, marked stale, with the
  last success time and the current error. The first failure of a source with no
  prior success is `unknown`/unavailable.

## Component status MVP (2026-10-19)

The Components list and each generic component detail now lead with one shared
evidence-bearing `status` projection. It reports the hub's actual observation
rather than inferring a state from a successful HTTP read:

- **Origin and generation.** A discovered package shows its exact installed
  generation. A conductor built-in has no installed generation and shows
  `none (built-in)`; the legacy synthetic binding generation zero is never
  presented as an installed generation.
- **Lifecycle and health are separate.** Lifecycle is the raw supervisor
  registry observation (`installed`, `starting`, `ready`, `draining`, `failed`,
  `retired`) with its observation time. Health comes only from an actual
  heartbeat matching the selected plugin and generation; no heartbeat means
  `Unknown`, not lifecycle-derived health. A failed/retired lifecycle stays
  prominent even when an older heartbeat said Ready.
- **Desired state is labelled as desired.** `Running (desired)` and
  `Stopped (desired)` never imply the observed process state.
- **Reports have their own availability.** `Fresh`, `Stale`, `Not reporting`, or
  `Unknown` describe the selected generation's report view. Stale values keep
  their original time and a visible stale label; they are never shown as a
  current healthy badge, and an incomplete view is called out.
- **Unknown stays neutral.** Absent reports, a cold detail failure with no
  retained data, and a missing status object all render explicit unknown or
  not-reporting text. A failed detail read keeps the route identity and shows
  either retained same-subject detail or, only when it is already loaded, the
  exact plugin-ID row from the Components page, labelled as a previous list
  observation. No other component's status is borrowed.
- **Diagnostics stay secondary.** Binding pins, schema descriptors and raw
  report envelopes remain in the existing collapsed details section. Declared
  generic views continue to render below the status summary.

The status object is optional on the wire: a response without it decodes as
unknown, so older fixtures and responses remain readable. Strict older Rust
readers may reject the added fields, so the matching hub and dashboard builds
must be deployed together; this increment makes no rolling-version compatibility
claim. Merged source is not proof that a running cockpit changed; activation
requires a separate coordinated restart of the matching hub and dashboard.

## Overview information hierarchy (UI.3)

The shared header appears on every route. It names the repository/state identity
from authoritative `factory.repository_id`/`state_id`, the typed runtime state,
the active page and the independent refresh state of the overview, factory and
components sources. Each source shows its own last-success relative age with the
exact ISO time; a retained failure is labelled stale, a first-read failure is
unavailable, and an unread source is not loaded. One successful endpoint never
claims the others are fresh.

Overview presents four compact cards (factory state, exact eligible total, health
**in the observed component page**, observed cost), then a two-column desktop grid
with dominant Current work, Needs attention, Eligible work, Economics details and
one Overview controls region. Current work uses only authoritative
`factory.current_execution`; a known null says “No current execution reported”,
an unavailable first read stays unknown, and a historical execution is never
substituted. Attempt information joins only on an exact execution-id **and**
generation match. Eligible work is a bounded five-entry preview in server order,
explicitly not a promised dispatch order, with the exact source total or an
unknown label.

Needs attention is deterministic display derived from source failures/unknowns,
typed factory state/stop, and observed components. Entries are deduplicated and
ordered error, warning, unknown; at most ten show with an exact additional count.
Component entries say they are page-scoped. Empty observations are described as
empty, coverage gaps stay separate, and read-only control permission is an
explanation beside the controls rather than a failure. Recent activity sorts
newest first, shows ten relative timestamps with exact ISO text, and expands to
the 100-entry bound. Technical IDs and execution history live in native
expansions whose open state survives polling and resets only when repository or
state identity changes.

Component names in the registry link to the existing encoded
`#/components/<plugin_id>` detail route and the detail back link returns to the
registry. Navigation, cards, disclosures and refresh submit no mutation. Control
permission, confirmation, action-id, pending, receipt and no-replay behavior are
unchanged.

UI.3 evidence adds generated `cockpit_information_*` cases to the `cockpit_page`
target: a 4x3x3 source/total/page state matrix, exact-decimal cases, six encoded
identity cases, eight disclosure/refresh sequences and a 4x2x3 viewport/theme
state matrix. The same focused run captures eight real browser screenshots
(populated Overview at 390/1280/1920 in dark and light, plus stale and
unavailable at 1280 dark) under the persistent evidence root. See
`docs/web-dashboard-design.md` section “Information and appearance increment
(2026-10-19)” for the canonical data rules.

## Refresh

One serial cycle runs at a time; the default interval is 1s, configurable
1–60s. Each request is abortable under a five-second deadline. A stopped or
superseded cycle publishes nothing, so a late aborted reply cannot overwrite a
newer cycle. Successful sources still update when another source fails. Leaving
the page stops timers and aborts reads. Reads, refresh, paging, reconnect and
reload never submit a control.

## Components and paging

Component pages default to 50 rows and are bounded to 100. Navigation uses the
explicit `Next` control with the server's opaque cursor. A `RevisionConflict`
resets the collection exactly once with a visible reload notice. Row state is
keyed by component identity **and** generation, so a generation replacement
drops the previous generation's row state. Plugin text is written with
`textContent`/`createTextNode` only, bounded to 4 KiB with an explicit
truncation marker; lists are bounded with explicit additional-content
indicators. Plugin HTML/scripts/templates are never evaluated.

## Generic declared component views (CW.1 / `agalma-51dg.2`)

`assets/views.js` renders the six declared `ViewKind` blocks —
Status, Metrics, Table, Timeline, Alert and ArtifactLink — for any registered
component. The renderer consumes only `ReadComponent` descriptors, report
envelopes and the active binding pin; it contains no component-ID or
private-schema branch. A previously unknown plugin therefore renders its
declared views without a consumer change.

Envelope selection is deterministic:

1. The route loader pins the detail subject and an epoch. A late response for
   another target, or an older epoch of the same target, is never committed;
   the page reads the pinned page, not a generic last-success value.
2. Every envelope for a descriptor namespace must match the page publisher and
   the full active binding pin (identity, API/version, implementation,
   generation, config digest). Identity `u64` values compare as exact decimal
   text; a legacy numeric generation is accepted only when safely integral.
   Conflicting identity or digest renders an explicit conflict.
3. The greatest sequence for a namespace is chosen **before** the descriptor
   schema version is checked. A superseded older version is never resurrected;
   the latest report with the wrong version is an explicit schema mismatch.
4. Views render in descriptor order. A missing report for the current page is
   `awaiting`, and any remaining opaque cursor is explained; without an entry
   the `reports` metadata state maps missing/empty/unknown to distinct honest
   states. Reports-missing is not registry-missing, and unknown is not empty.
5. Field lookup is own-property only and supports dotted paths. `null`, `false`,
   zero, empty string and an absent field each keep a distinct presentation;
   composite values render as bounded JSON text.

Presentation keeps the existing bounds: 4 KiB per text value, 100 rows/entries
per report page, and 24 table/timeline rows by default with an explicit
expanded disclosure for the remainder. Technical schema/binding metadata and raw
payloads live under native `<details>` elements keyed by subject, binding
generation and view ID, so their open state survives polling and resets when the
subject or generation changes. Status colors always carry their exact text; an
unrecognized status or alert severity stays unclassified. ArtifactLink renders
label/reference/digest as inert text and states that download is unavailable;
it emits no `href`, no `src` and initiates no request. Focus and scroll remain
stable across refresh.

The page keeps registration as a registry-only fact: a built-in with no reports
still shows its binding and declared views, while only the registry `missing`
source renders “not registered”. A component page with no declared views shows
“No declared views.” and one with no report envelopes shows “No report
envelopes.”.

## Optional factory endpoint and controls

`GET /api/v1/factory` and the control routes are optional in M1. When the route
is absent the page shows the runtime as explicitly unavailable and disables both
controls. When a typed factory runtime advertises permissions, `controls.js`
exposes only the advertised operations:

- Pause sends `{format:"FactoryControl/v1",operation:"pause",action_id,confirmation:null}`.
- Resume requires the existing literal confirmation phrase `resume`.
- One explicit click generates and retains one action id and disables duplicate
  clicks while pending.
- A lost response queries the retained id against the durable receipt and shows
  `EffectUnconfirmed`; the browser never replays the mutation automatically.
- Disabled when read-only, unknown or offline. Loading/refresh/navigation never
  POSTs.

## Evidence

Run the focused selections separately (both must pass; a missing case is a
failure):

```sh
CHROME_BIN='<installed chrome>' make test-focused \
  SELECTION='-p agalma-web-dashboard --test cockpit_page'
CHROME_BIN='<installed chrome>' make test-focused \
  SELECTION='-p agalma-web-dashboard --test first_launch'
```

`cockpit_page` runs the retained focused regressions in the installed browser
against a real loopback fixture server with independent `fixtures/cockpit` JSON:
bounded
safe rendering with exact identities; inert injected text; stale-source
retention and conflict reload; bounded component paging; permission-gated
controls with receipt lookup and no automatic replay; and page-exit timer and
request-abort cleanup. Its component-detail coverage includes a deterministic
12-case registry (available/missing/unreadable) x reports
(available/empty/missing/unknown) source-state matrix and a concrete
`component:harness` no-report binding regression. The broader generated/
out-of-order state-machine corpus is not part of WEB.6 release acceptance.

CW.1 adds the generic-view selections (run them with the same environment):

```sh
CHROME_BIN='<installed chrome>' make test-focused \
  SELECTION='-p agalma-web-dashboard --test cockpit_page cockpit_component_views_'
CHROME_BIN='<installed chrome>' make test-focused \
  SELECTION='-p agalma-web-dashboard --test cockpit_page cockpit_page_component_detail'
CHROME_BIN='<installed chrome>' make test-focused \
  SELECTION='-p agalma-web-dashboard --test first_launch first_launch_production_'
```

`cockpit_component_views_` runs eight installed-browser cases against real
compiled assets: a 6-kind x 6-data-state matrix (36 scenarios), 16 dotted
field/value boundaries, 12 sequence/version/binding permutations, eight
route-switch/epoch traces, a 4-width x 2-theme layout matrix with expanded long
tables and local stale labels, an unknown-plugin/private-namespace case, an
injection/exact-identity case, and eight real browser screenshots
(populated at 390/720/1280/1920, stale, unknown and expanded at 1280 dark).
Deterministic scenario IDs and case listings are written beside the screenshots
under `target/test-runs/agalma-51dg.2/` (the `RUN_ROOT` persistent root), and the
runner keeps its raw logs under the same bead directory.

`first_launch` is the real production smoke: an isolated established ledger, the
sealed installed package, the assembled production hub
(`PluginHost::assemble_with_ledger`) serving the real `control-plane.read/v1`
router, the real supervised dashboard process, and the installed browser opening
the printed URL. It asserts real component data renders, the installed
dashboard's encoded detail route renders its binding and declared views, the
conductor built-in `component:harness` and a valid unknown id return the honest
detail projection, the factory endpoint is honestly unavailable with controls
disabled, a lost read source surfaces as `503` rather than a healthy empty page,
and viewing creates no control intent or task claim (the child holds exactly
`control-plane.read/v1`; no control route exists).

Run artifacts are written under `target/test-runs/<bead>/<run-id>/` and are
never deleted. CW.1 additionally keeps its scenario listings and screenshots
under `target/test-runs/agalma-51dg.2/` at the persistent root.
`browser_support/` is the shared harness; later WEB.5/7/8 readers consume it
after a named ownership transfer rather than building a replacement.

## M2: continuous automatic-queue operation and operator controls (WEB.13)

M2 (`agalma-rwn.13`) joins the reviewed runtime, factory adapters, bounded HTTP
controls and this cockpit package into one production local automatic-queue
cockpit. The page is unchanged; the runtime and the control surface are wired
end to end.

### The one production command

```sh
# One repository/state directory, one continuous worker, one operator cockpit.
agalma work --watch \
  --repo <repo> \
  --state-dir <state> \
  --plugin-root <packages>
```

`--watch` is the continuous single-owner mode. It rejects `--once` and
`--max-tasks`. The runtime holds the state-directory admission lock for the
service lifetime; a second `agalma work` against the same state directory is
refused. Ordinary batch/once/max-tasks behavior and exit codes are unchanged.

Eligible-work behavior: the worker selects/claims already-eligible local tasks
through the existing pipeline and consumes them serially. An empty successful
queue is `Idle` with an interruptible five-second wait; a later eligible task is
consumed automatically. A queue error is `Unavailable` with capped retry (5s
initial, 30s maximum), never a successful empty state. Queue polling never
creates, releases or re-approves a task.

### URL discovery and control grant setup

The supervised dashboard prints its actual loopback URL on stderr once the
listener and read binding are ready, and reports the same URL in `plugin.Health`
as `;url=`:

```text
DASHBOARD_URL http://127.0.0.1:<port>
```

Port `0` (default) selects a free port, so the printed value is authoritative.
The dashboard is read-only until the operator-managed explicit installation pin
approves `factory.read/v1` and `factory.control/v1` for the sealed package (in
addition to `control-plane.read/v1`). The configured-root auto-install path
grants only `report.publish`/`ledger.read`, so an unapproved install exposes no
factory route and disabled controls. `factory.control/v1` is **not** in
`SAFE_CAPABILITY_ALLOWLIST`; control authority is always an explicit operator
decision.

For the continuous `agalma work --watch` runtime, the operator names the single
control surface explicitly:

```sh
agalma work --watch --repo <repo> --state-dir <state> \
  --plugin-root <packages> --factory-control-plugin agalma.web-dashboard
```

Only the installed plugin with that exact id is considered. Its **current
(newest installed) generation** pin is read from the existing durable hub
installation store
(`<state_dir>/plugins/<plugin_id>/<generation>/installation.json`) — the same
pins the production read model already consumes. Only the current pin is
consumed (no older-generation fallback), so a newer generation whose pin drops
`factory.control/v1` revokes an older grant. The pin is accepted only when its
frozen `package_digest` matches the discovered package bytes **and** it
explicitly approves `factory.control/v1`; assembly then re-validates it
(configuration artifact, launch profile and requested-capability subset).

Absent `--factory-control-plugin` keeps the cockpit read-only, and any
unselected plugin never receives control (its explicit control pin is ignored).
An absent, changed, mismatched or non-granting current pin fails closed to the
read-only safe-allowlist auto-install path; no selector, no plugin id in source,
and no expansion of `SAFE_CAPABILITY_ALLOWLIST` is introduced. A selected plugin
with a qualifying pin is also requested `Running`, so the runtime discovers,
installs, launches and supervises it (its actual URL then appears on the
runtime's stderr).

Recording the pin is the operator's use of the existing install API
(`HubInstallStore::install`) — there is no separate approval file, environment
map or id-only allowlist, and no change to `SAFE_CAPABILITY_ALLOWLIST`.

When approved, the launcher issues distinct per-capability grants and a generic
private `PluginCapabilityBindings/v1` descriptor on the launch channel (never in
a browser response, report or log). The dashboard then mounts:

| Route | Behavior |
|---|---|
| `GET /api/v1/factory` | factory runtime snapshot; bootstraps the operator session |
| `GET /api/v1/factory/actions/{action_id}` | authoritative receipt lookup |
| `POST /api/v1/factory/pause` | operator pause |
| `POST /api/v1/factory/resume` | confirmed resume (literal phrase `resume`) |

POSTs require a same-origin `Origin`, the `agalma_operator` cookie and the
`x-agalma-csrf` header from a same-origin `GET /api/v1/factory`. Reads, refresh,
navigation, reload and reconnection never submit a control. The hub-side adapter
is the single serialized control owner; it holds a state-scoped
`factory-control.lock` for the service lifetime, and a competing owner disables
controls (fail closed). Export a valid factory control grant to the dashboard
through the supervisor's normal installed-package launch; there is no
browser-supplied grant.

### Pause versus shutdown

`Pause` blocks new admission through the existing stop policy (the durable kill
latch/sentinel); it is not a cancel and does not change in-flight phase-stop
semantics. `Resume` requires the existing literal `resume` confirmation and a
durable receipt; a Resume that cannot confirm STOP removal reports an
unconfirmed outcome (`EffectUnconfirmed`), never a claimed success. Graceful
shutdown (SIGINT/SIGTERM) is distinct: it stops admission, drains at the existing
cooperative phase boundary, joins the host owner and releases the lock. A restart
with an established STOP/latch starts `Paused` and exposes an explicit resume;
fresh, missing, corrupt or incompatible established storage is never initialized
or replaced under a stop.

### Failure and restart

Frozen-package recovery runs once at admitted startup before new claims; forced
termination can leave durable nonterminal work, recovered on the next permitted
startup from the frozen package. A fatal worker failure is explicit and requires
a runtime restart; plugin-child restart never creates a second queue claimant.
The durable control records are `factory-control.intent/<action_id>` and
`factory-control.result/<action_id>`: an identical replay returns the frozen
receipt, a conflicting reuse of an action id is refused (`409`), and an
interrupted effect leaves `EffectUnconfirmed` (browser does not auto-retry).

### Local-only limits

Bind is `127.0.0.1` with port `0` by default; foreign addresses are refused.
There is no remote access, account system, per-task dispatch button, task/backlog
editing, generic plugin action or artifact route, and no new framework or
dependency. The cockpit reports only what the bound APIs expose; unknown costs,
worker state and source availability stay explicit, and a missing stop authority
is never shown as a green Running indication.
