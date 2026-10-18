# HP bead readiness review and handoff amendment

Date: 2026-10-13. Applies to `agalma-jmr.1` through `agalma-jmr.8`.
This amends implementation ownership and observable acceptance. Revision-1
protocols in `self-mutation-contracts.md` and their canonical schema/examples
remain authoritative. Historical design receipts are unchanged. All children
remain deferred; review preparation grants no implementation or spending release.

## Production ownership

| Bead | Production responsibility |
|---|---|
| HP.1 | Canonical package/installation, lifecycle, publication/envelope, operation/cancel, component and complete-state row contracts; no runtime I/O |
| HP.2 | Root selection through existing CLI/configuration, validated inventory and discovery descriptors; no process launch |
| HP.3 | Hub-owned immutable installed generations, registry, supervisor and production host assembly/startup/shutdown; explicit injected persistence and RPC seams |
| HP.4 | Real Connect adapters, concurrent dispatch, authenticated capability routing and production host RPC attachment |
| HP.5 | Selected LedgerApi component store, durable registry persistence, report ingestion and production host attachment; complete-state migration/restore |
| HP.6 | Shared registry/report read model, status and declarative rendering; first ownership of watch.rs |
| HP.7 | Action service, hub action routing, control-plane attachment and action-specific production host composition; watch.rs action hookup after HP.6 hands it over |
| HP.8 | Independent executable package build and proof against the production host/UI/action paths; no substitute test-only hub |

HP.2 additionally owns conductor `src/cli.rs` and `src/config.rs` for common root
configuration. HP.3 additionally owns `src/cli.rs`, `src/config.rs` and
`src/work.rs` for necessary host entry/lifecycle integration. Binary dispatch
already lives in `src/lib.rs`; `src/main.rs` remains a thin entry point.
Do not introduce a second scheduler, paid inference default, per-plugin selector,
or new configuration framework. A configured reporting plugin must be operable
without claiming a paid factory task. HP.3 owns the common host entry and
documents its exact invocation. HP.8 uses that invocation.

### HP.7 production action attachment addendum (2026-10-17)

With explicit user approval, HP.7 additionally owns action-specific additions
to `src/composition.rs`: construct the registry-derived action catalog and
pinned-generation executor from the running `PluginHost`, attach durable action
storage to the selected ledger, supply each active plugin's launch grant, and
pass the validated service-backed driver to the interactive UI. The ordinary
UI path remains fail-closed when no host service is supplied. This ownership
does not transfer unrelated host lifecycle, CLI configuration, or ledger
ownership, and HP.8 remains proof-only for production implementation files.

HP.3 tests its injected seams and identifies their production attachments as
pending. HP.4 attaches real RPC; HP.5 attaches durable selected-ledger storage and
reporting. Earlier component acceptance does not establish durable production
restart or complete platform readiness. HP.5 must prove the assembled host reads
and writes registry state through the selected LedgerApi, without a private file
database or direct SQLite fallback. Built-in representations do not claim full
service migration; that remains CV work.

## Root policy and package identity

No configured roots means an empty discovery set. Explicit roots are required
unless marked optional in common root configuration. A missing required root
refuses discovery. A missing optional root yields an empty contribution and a
visible diagnostic; discovery must not create that directory. Permission errors,
non-directory roots, malformed packages, digest mismatches, escaped paths and
duplicate IDs remain explicit failures, including under optional roots.

HP.2 tests this policy, deterministic inventory/digest validation, and config
round trips. HP.3 owns copying validated packages into hub-owned immutable
generation storage and rehashing on install/stage/launch. Tests edit the discovery
directory after installation and prove an existing binding is unaffected.

## Required acceptance additions

- HP.1: independent fixtures include PluginInstallation, separate launch epoch
  and logical generation, ReportPublication versus ReportEnvelope, stable cancel
  IDs, component replay anchors and all complete-state/v2 row sections.
- HP.3: injected-clock lifecycle cases include Health-gated readiness,
  restart fencing, durable desired-state interface use, active/pending generation
  separation, drain refusal with unresolved operations and immutable install pins.
- HP.4: a real Connect callback begins before its enclosing action can complete;
  both finish within a bounded deadline. Cross-client numeric request IDs cannot
  collide as durable action identity. Changed operation input is refused.
- HP.5: explicit v4-to-v5 migration refuses non-quiescent state and performs no
  upgrade on normal open. A successful migration has backup and receipt evidence.
  Complete-state/v2 round trips contain nonempty decisions, decision outcomes,
  digests, control receipts and component checkpoints/events/operations/receipts/
  commit anchors. After restore, identical replay returns its original receipt;
  changed input conflicts. Test atomic failure, unknown sections, legacy-v1
  production restore refusal, and both ledger transports with real binaries.
- HP.6: report-rendering tests include control-character escaping and bounded
  rows/text; viewing and refresh remain read-only.
- HP.7: service tests can proceed while HP.6 owns watch.rs. UI dispatch integration
  waits for an explicit file handoff. The production host passes a service-backed
  driver, registry-derived action descriptors, per-launch grants and pinned
  executor to the interactive UI. Unknown outcomes retain the original plugin
  generation; cancellation evidence cannot be inferred from a request alone.
- HP.8: build the hub once, then independently build/install plugin generations;
  exercise the actual host, selected ledger, generic UI and authorized action
  service. Record hub/package/config digests and independent effect journals.

## Parallel work and handoffs

Retain the existing dependency graph: HP.1 -> HP.2 -> HP.3 -> HP.4 -> HP.5,
then HP.6/HP.7, then HP.8. No task is released by this review.
Shared conductor `lib.rs`, `cli.rs`, `config.rs`, `composition.rs`, `work.rs`,
registry/supervisor and IPC files transfer only after the earlier owner finishes.
SF owns overlapping files until its accepted integration is complete.
HP.6 owns `watch.rs` first; HP.7 develops its service and tests concurrently, then
receives `watch.rs` for action hookup. HP.3's host composition becomes available
for HP.7's action-specific production attachment after HP.3 lifecycle acceptance.
Both acceptances must complete before HP.8.

## Independent plugin fixture and checks

HP.8 owns `fixtures/plugin-pack/` including Cargo.toml, Cargo.lock, source,
manifest, configuration schema, default configuration, package resource inventory
and README. The fixture is a separate executable using public contracts/IPC,
with its own Cargo workspace boundary, existing pinned dependencies and no new
downloads. Package generations and run logs belong under `target/test-runs/`.

The separate build command is:

`cargo build --offline --manifest-path fixtures/plugin-pack/Cargo.toml --target-dir target/plugin-pack-build`

This command is a future implementation requirement, not a current executable
fixture or passing check. Reuse that isolated cache. Build the hub before the
fixture; do not rebuild it between plugin generations. The test must execute
required local processes; absent executables or zero-case selections fail.

Keep each bead's existing focused Cargo test target as its acceptance entry.
Name necessary binary build prerequisites separately. HP.8 runs its focused
proof first, then one full workspace acceptance with architecture checks and
documented environment requirements. Keep paid-provider checks outside this
platform proof; there is no provider inference requirement here.

Each implementation handoff names its bead, canonical paths, owned files,
required cases, exact commands/results, artifact location and unresolved pins.
Read the complete assigned bead locally; inspect related state compactly instead
of loading complete parent/dependency bodies. For an implementation blocker,
capture one reproducible case and stop investigation after 30 minutes with a
bounded handoff. Do not redesign contracts, acquire another owner's files,
commit/push, close successors or infer release from a cleared defer date.
