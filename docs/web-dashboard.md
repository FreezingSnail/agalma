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
  `api.js`, `refresh.js`, `controls.js`
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
| `GET /api/v1/factory` | optional factory runtime snapshot (see below) |

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

`cockpit_page` runs six focused regressions in the installed browser against a
real loopback fixture server with independent `fixtures/cockpit` JSON: bounded
safe rendering with exact identities; inert injected text; stale-source
retention and conflict reload; bounded component paging; permission-gated
controls with receipt lookup and no automatic replay; and page-exit timer and
request-abort cleanup. The broader generated/out-of-order state-machine corpus
is not part of WEB.6 release acceptance.

`first_launch` is the real production smoke: an isolated established ledger, the
sealed installed package, the assembled production hub
(`PluginHost::assemble_with_ledger`) serving the real `control-plane.read/v1`
router, the real supervised dashboard process, and the installed browser opening
the printed URL. It asserts real component data renders, the factory endpoint is
honestly unavailable with controls disabled, a lost read source surfaces as
`503` rather than a healthy empty page, and viewing creates no control intent or
task claim (the child holds exactly `control-plane.read/v1`; no control route
exists).

Run artifacts are written under `target/test-runs/agalma-rwn.6/<run-id>/` and are
never deleted. `browser_support/` is the shared harness; later WEB.5/7/8 readers
consume it after a named ownership transfer rather than building a replacement.
