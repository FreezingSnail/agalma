# Browser dashboard and shared plugin display interface

Status: accepted for decomposition, 2026-10-13. The user approved this design
and requested bead decomposition. Implementation has not been released.

## Goal and decisions

The user wants to view the Agalma dashboard in a browser and have a standard
interface through which any plugin can display information there.

Accepted first delivery: a local, read-only dashboard with factory overview,
executions, component health, economics, timeline, and plugin detail pages.
Remote access and operator controls need their own explicit delivery scope.

The common plugin reporting and view contracts already exist as revision-1
designs. This module consumes them. Their runtime implementation remains staged
under the HP plugin-platform work.

## Module boundary

Package the dashboard as an independently installable plugin process. Its package
contains its executable, browser assets, manifest, configuration schema, and
protocol version. The common registry and supervisor discover and launch it.
It reports its own health through the common plugin contract.

The process serves the browser assets and a bounded HTTP read adapter. It receives
scoped read capabilities through the hub. The browser talks to this adapter; the
adapter reads hub-owned projections through declared APIs and hub routing.
Credentials used for plugin-to-hub access stay inside the server process.

The dashboard opens no ledger database and discovers no peer sockets. It uses
the same canonical identities and report projections as other frontends.
Refresh is observational: it does not claim tasks, trigger builds, or dispatch
plugin actions.

Initial transport is periodic snapshot refresh, with a visible last
successful refresh and connection state. Streaming can follow if a measured
need justifies it. A failed refresh retains the last snapshot marked stale;
it must not turn missing information into healthy or empty state.

## Standard display interface

The existing canonical sources are:

- `docs/self-mutation-contracts.md`, HP reporting and lifecycle sections.
- `docs/contracts/self-mutation-v1/schema.json`, especially `PluginManifest`,
  `View`, `ReportPublication`, and `ReportEnvelope`.
- HP.1 (`agalma-jmr.1`) for typed contracts and HP.5 (`agalma-jmr.5`) for
  durable reports and snapshot recovery.

Each plugin declares namespaced, versioned report schemas and view descriptors
in its manifest. It publishes data conforming to those schemas. A descriptor
contains a stable view ID, title, schema namespace/version, supported block kind,
and field paths. The frontend binds those paths to the plugin's reported data.

The supported blocks are Status, Metrics, Table, Timeline, Alert, and ArtifactLink.
For example, a build plugin can publish build rows and a manifest Table view;
the browser dashboard then displays those rows using its generic table renderer.
That plugin needs no custom dashboard code.

Descriptors contain bounded data and field paths. They contain no executable
scripts, templates, SQL, or remote fetch instructions. Plugin text is rendered
as text and rows/text are bounded under the common contract. Unknown namespaces
have a generic data fallback; unsupported view versions are shown explicitly.
Artifact links use authorized artifact references resolved through the hub.

Report identity, binding generation, sequence, timestamps, and recovery watermark
remain authoritative. The hub performs validation, deduplication, gap detection,
and snapshot recovery. The dashboard displays the resulting completeness and
freshness state rather than independently rebuilding producer history.

Navigation derives from registry entries. An installed plugin appears with its
name, health, and declared views. Degraded, unavailable, stale, disconnected, and
unsupported states remain distinguishable. An empty successful report differs
from unavailable data.

## Relationship to existing UI work

HP.6 (`agalma-jmr.6`) currently adds registry navigation and generic rendering to
the TUI. The browser module is a second renderer of the common report/view
contract. Keep report validation and read-model derivation shared; each renderer
owns its presentation.

The existing `status.rs` path opens SQLite and derives a fixed component list.
These are current migration gaps, not a recipe for this module. CV.2
(`agalma-pob.2`) tracks standalone API convergence. The browser plan needs a
declared hub read surface for registry/report projections and factory overview;
the detailed design must identify its ownership before implementation. Avoid
adding a circular dependency between that shared read surface and dashboard
acceptance.

Operator actions remain advertised separately from reports. If browser controls
are included in a later agreed increment, they use HP.7 (`agalma-jmr.7`) admission,
pinned operations, durable receipts, cancellation, and reconciliation. Viewing a
plugin never grants action authority.

## Implementation contract for decomposition

Use a new `agalma-web-dashboard` crate for the plugin executable. Package browser
assets with that executable; serve plain HTML, CSS, and JavaScript with browser
modules. WEB.4 generates the compiled asset inventory from committed assets
using std-only build logic; later asset modules require no server source edit.
An initial health page reflects real listener/read-binding status. No npm build
chain or UI framework is required. HTTP plumbing stays in
`agalma-ipc`, using the locked transport dependencies; the dashboard depends on
public contracts and IPC, never on conductor or spoke implementations. New direct
dependencies or downloads need a separately reviewed dependency decision.

Add a typed hub-owned `ControlPlaneReadApi/v1` behind HP.4's capability routing.
The dashboard requests `control-plane.read/v1`. This capability authorizes
observation and report-referenced artifact reads only. It grants neither arbitrary
LedgerApi calls nor control actions. The hub derives scope from the authenticated
plugin grant, not browser-supplied identity. Revoked/expired grants refuse reads.
Reads remain available while factory effect admission is stopped.

The API owns these read operations and projection rules:

| Operation | Result and source |
|---|---|
| ReadOverview | Factory stop observation, executions, known usage/cost and allowance, curated timeline; sources are bound domain APIs |
| ListComponents | Registry identities, active generations, health/freshness/completeness, declared views; includes installed unavailable components |
| ReadComponent | A component's bounded namespace reports and descriptors from the authoritative HP.5 projection, retaining envelope identities |
| ReadArtifactChunk | At most 64 KiB from an immutable, authorized report reference; total artifact bound remains 16 MiB |

Read response format is `ControlPlaneRead/v1`; its version is independent of
ledger schema version. Each successful response carries collection time, source
availability, registry revision, and report watermark where applicable. A page
contains at most 100 entries and at most 256 KiB serialized JSON. Limit defaults
to 50, accepts 1-100, and never permits unbounded reads. Opaque cursors pin the
collection/filter/revision; invalid cursors fail and expired revision cursors
return RevisionConflict so the client starts a fresh collection. No implicit
claim of atomicity across different domain sources is made: their availability
and observation metadata remain visible.

Represent identity-bearing u64 generations, sequences, revisions, watermarks,
and money as decimal strings in the browser read projection to retain exact
values. HP producer envelopes and their revision-1 schema remain unchanged.
Unavailable usage or allowance is unknown, never a numeric zero. A missing
ledger, unreadable ledger, empty successful list, and stopped factory are distinct
states. Source failures do not initialize or reopen private storage.

The hub implementation reuses HP registry/report projections and existing pure
factory calculations through bound API inputs. HP.6 retains TUI ownership;
CV.2 retains standalone CLI migration. The new read handler is registered once
as a shared platform service. Adding reporting plugins requires no new handler,
import, environment selector, or frontend entry. Runtime wiring filenames below
are intentional targets after HP lands, not claims that those modules exist now.

The browser HTTP adapter maps GET `/api/v1/overview`, `/api/v1/components`, and
`/api/v1/components/{plugin_id}` to the typed reads, and
`/api/v1/artifacts/{plugin_id}/{digest}` to authorized chunk reads. Plugin IDs are
URL-encoded opaque IDs, not filesystem paths. Artifact requests resolve against
references visible in that plugin's authorized reports and revalidate authority;
clients cannot select an arbitrary store path or remote URL. The server returns
explicit errors for unknown routes, unsupported methods, versions, limits,
scope, and unavailable sources. The API grants no mutation method. Browser
responses contain no hub grant token, provider credential, or private config.
Report artifacts download as attachments with content sniffing disabled; plugin
HTML or scripts never execute under the dashboard origin.

Listen on `127.0.0.1` only; default port 0 lets the operating system select an
available port. Report the actual local URL and listener health through the
common plugin contract after both listener and hub read binding are ready. Allow only the
configured local Host/Origin, use same-origin browser fetches, and enable no
cross-origin reads. Configuration rejects non-loopback addresses. Read deadlines
are 5 seconds; at most 16 HTTP reads are outstanding, with bounded backpressure.
Listener shutdown drains admitted reads within that deadline and closes cleanly.

Field selectors in `View.fields` retain the existing bare top-level key form
(for example `ready`). A selector beginning `/` is an object/array path with
`~0` and `~1` escapes for literal tilde and slash. Other selectors are literal
top-level keys. They are never expressions or executable templates. Missing
fields render as missing, never healthy. The generic browser renderer applies:

| Block | Resolved field value and presentation |
|---|---|
| Status | Scalar or bounded object, labeled by field name; plugin health remains a separate hub observation |
| Metrics | Scalar or `{value, unit}`; decimal values are text and unknown values remain unknown |
| Table | Array of objects, columns from sorted keys; scalar rows use a Value column; each field selects its own table |
| Timeline | Ordered array of entries; preserve producer order and display available time/title/detail fields |
| Alert | Text or `{severity, message}`; unsupported severity is visibly unknown |
| ArtifactLink | Canonical artifact reference resolved through the authorized adapter |

Incompatible field shapes render a bounded generic data fallback with a visible
reason. Unknown view versions also show a fallback and their version. Bound a
rendered field to 4 KiB text, a table/timeline to 100 rows, and a table to 20
columns per page; indicate additional content explicitly. Full authorized data
remains reachable through pagination or artifact references under common bounds.
Render text with DOM text operations; never evaluate report text as HTML/code.
These renderer semantics interpret existing View fields and do not rewrite the
frozen HP schemas or receipts.

The shell provides Overview, Executions, Economics, Timeline, and Components.
Component navigation is registry-derived, keyed by stable plugin identity and
generation. Selection survives refresh while its identity remains present.
Plugin removal leaves a visible unavailable selection and permits navigation
away; it does not display cached data as current. The Overview includes factory
stop state and source freshness. There are no action controls in this delivery.

Poll every 1 second by default, configurable from 1-60 seconds. Do not overlap
refreshes. Reject late responses from an older refresh or hub/registry revision;
a generation change replaces the previous generation's current data. A failed
read retains the last successful data marked stale with its timestamp and error.
On restart or RevisionConflict, reacquire the registry and fresh report snapshot.
Gap recovery remains HP.5's responsibility; the browser displays incomplete state.

Ownership and dependency order:

| Child | Owned surface | Required predecessors |
|---|---|---|
| WEB.1 | Typed read contracts and independent wire fixtures | HP.1 |
| WEB.2 | Hub projection/capability handler and scoped read integration | WEB.1, HP.4, HP.5, HP.6 |
| WEB.3 | Loopback HTTP transport abstraction in agalma-ipc | WEB.1, HP.4 |
| WEB.4 | Dashboard executable, manifest/config, packaged assets/lifecycle | WEB.2, WEB.3, HP.3 |
| WEB.5 | Generic view-block renderer and browser fixture harness | WEB.4 |
| WEB.6 | Dashboard shell, overview and registry navigation | WEB.5 |
| WEB.7 | Refresh, stale/disconnected state and restart recovery | WEB.6 |
| WEB.8 | Independently built unknown-plugin browser proof and operator docs | WEB.7, HP.8 |

WEB.2 owns shared conductor registration edits after HP.4/HP.6 handoff. WEB.3
owns IPC exports after HP.4. WEB.4 owns workspace/Cargo/package wiring; successor
UI children may not change those files. WEB.5 introduces the browser fixture
harness, transferred to WEB.6 then WEB.7 then WEB.8. Assets are split into
renderer, shell, and refresh modules with the same order. Shared integration
edits are limited to exposing the assigned surface; preserve unrelated work.

No dependency points from HP or CV back to WEB. The browser proof follows HP.8;
it does not become an HP prerequisite. The browser can be planned and compiled
in parallel with platform work only within explicit file ownership; staged
dependencies and implementation release still govern dispatch.

## Delivery and acceptance

The bead index is `docs/web-dashboard-beads.md`. Children use existing focused
Cargo commands during the test-runner transition. Browser tests use an installed
Chrome/Chromium executable supplied as `CHROME_BIN`; no browser/tool download is
assumed. Required browser cases must fail on a missing executable, a skipped case,
or zero-case selection. The implementer owns reusable repository test code,
including browser fixtures; Sol adds no disposable executable wrapper.

Each implementer reads its full bead, this design, AGENTS.md, and the workflow
locally. Implementation belongs to GPT-6 Luna or DeepSeek Flash through OpenCode.
Stop after focused acceptance and a concise handoff, or one reproducible blocker
and at most 30 minutes of investigation. Artifacts go under target/test-runs;
preserve exact commands, raw logs, exit status and executed/ignored case counts.
Return predecessor contract/wiring defects to their owner instead of acquiring
another child's files. Proposed intake budgets are ceilings for later release,
not authorization to spend. Decomposition approval releases no implementation.

The implementation follows the HP dependency order: contracts, discovery and
registry, hub routing, report ingestion/recovery, then browser rendering. Reuse
existing work rather than duplicate those platform tasks.

Browser acceptance must use the production registry and report path:

1. Install and launch the dashboard package through common discovery/supervision.
2. View factory state and a previously unknown reporting plugin in a real browser.
3. Add that reporting plugin without editing or rebuilding the hub or dashboard.
4. Render all six supported blocks with independent fixture data; cover malformed
   descriptors, unsupported versions, bounded content, and escaped plugin text.
5. Show stale/disconnected state after report or hub loss; recover from a fresh
   snapshot without retaining data from a retired generation as current.
6. Restart the hub/dashboard and retain plugin identity and durable observations.
7. Establish that viewing and refresh do not create task claims or action intents.

Scoped contract/adapter checks come first. WEB.8 runs the browser proof and then
one documented full workspace build/test acceptance, recording the broader cost
before execution. Isolated renderer fixtures do not establish production plugin
installation. No paid-provider check is required. Planning validation and content
review do not claim implementation or browser acceptance.
