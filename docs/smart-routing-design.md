# Smart routing v2 — engine- and budget-aware cost governor (design)

Status: planning design. Date: 2026-10-12. Epic: `agalma-myx`.
Supersedes the static policy shape of H3 (`docs/reliability-hardening.md` §H3,
`crates/agalma-conductor/src/routing.rs`); v1 is retained as the fallback
baseline and the comparison arm. Routing ships as a **nano service** (Router) on
the hub-spoke topology: `docs/component-contracts.md`,
`docs/ipc-connect-design.md`.

## 1. Goal

Maximize accepted merges per unit of cost across every inference engine and
billing mode Agalma can reach:

- do not burn scarce rolling-subscription quota on work that free or cheaper
  adequate capacity can do;
- do not let subscription allowance expire unused at a window reset when it
  would have replaced cash;
- do not pay peak rates for deferrable work;
- never route a model whose cost or billing class is unknown.

Metrics (recorded per run, ledger-derived):

| Metric | Definition | Target |
|---|---|---|
| cash $/merge | real dollars per merged task | no regression vs v1; lower on peak-heavy workloads |
| quota efficiency | qualified merges per $ of allowance consumed | up vs v1 |
| window waste | allowance unconsumed at reset while cash was spent in-window | 0 |
| quota-blocked attempts | 429/quota exhaustion mid-attempt leading to a lost attempt | 0 |
| peak cash share | cash spend inside peak windows on deferrable tasks | ~0 |

## 2. Current state (H3 v1)

- Static `MODEL_TABLE` (10 models, OpenCode Zen/Go only) plus route classes
  (builder/verifier/planner) with fixed preference lists.
- Paid gate: `AGALMA_ALLOW_PAID=1` **and** a present `opencode-go` credential;
  `--model` override; every selection recorded as a `route.model` decision.
- One engine: OpenCode (`agalma-harness-opencode`); the harness is a
  process-global kind (`--harness`), not a per-route property.
- Per-turn `cash_usd` / `quota_usd` are recorded, but there is no window state,
  no pacing, no peak clock. DeepSeek rows pin off-peak prices only; peak/off-peak
  is not classified at request time.
- Routing is a conductor module, not a boundary. This epic lifts it into the
  **Router** nano service behind `RouterApi` (spoke), consumed by the conductor
  (hub) exactly like the other components.
- `economics.rs` allowances are display-only; "no budget enforcement" is a
  current design non-goal. This epic turns **quota** into a routing input (cash
  spend stays report-only).

## 3. Fact base (verified 2026-10-12)

| Engine | Provider(s) | Billing | Rolling windows | Notes |
|---|---|---|---|---|
| OpenCode (`opencode serve`) | `opencode` (Zen) | pay-per-token cash | none documented; free-model rate limits unknown | free models are limited-time; some train on data |
| OpenCode | `opencode-go` | $10/$40 per month; at-cost token rates charged against a per-model $ allowance | 20% per 5h, 50% weekly, 100% monthly, per model | window exhaustion blocks requests (429) unless Zen-balance fallback is enabled |
| Codex (`codex exec`) | ChatGPT plan (OAuth `~/.codex/auth.json`) | plan allowance | 5-hour rolling + weekly, model- and plan-dependent; credits purchasable | CLI `/status` and status line expose window usage |
| Codex | OpenAI API key via Codex | pay-per-token cash | RPM/TPM per project | the documented automation default |

Host inventory: `codex-cli 0.162.0` (configured model `gpt-6.1-sol`; status
line includes `five-hour-limit`, `thread-credits`, `estimated-thread-cost`) and
`opencode v2.0.15`; both credential stores present.

DeepSeek peak clock (billing): peak = Mon–Fri 01:00–04:00 and 06:00–10:00 UTC
(2× rate), Chinese public holidays excluded; all other hours and weekends are
off-peak. Go allowance drains at peak rates inside these windows, so peak
avoidance helps quota as well as cash.

Rolling-window semantics that must be modeled, not assumed: Go limits are
per-model $ allowances split 20/50/100 across 5h/week/month windows; Codex
limits are percentage windows with provider-reported resets. Both are
**use-it-or-lose-it**: headroom at reset is lost.

## 4. Design

### 4.1 Router nano service boundary (SR.11, SR.12)

Routing becomes a first-class nano service — the **Router** — on the hub-spoke
topology: the conductor is the hub; the Router is a spoke reachable only through
its contract.

- **Contract**: `RouterApi` in `agalma-contracts` (canonical IDs, requests,
  results, events, errors; no vendor types). Operations: `Describe`/`Health`,
  `EvaluateRoute` (idempotent by `(scope, policy revision, budget-snapshot hash,
  clock)`; returns the chosen route, per-option scores, elimination reasons, and
  the durable decision receipt), `BudgetSnapshot`, `CatalogSnapshot`/`Refresh`,
  `RecordObservation` (provider-reported window state), `Explain`.
- **Implementation**: `crates/agalma-router` (new crate, subject to the
  dependency policy). Registry rows, price schedules, window derivation, and
  policy evaluation are private to it.
- **State**: the Router owns no database. Route decisions and budget
  observations are durably committed through `LedgerApi`; in-memory snapshots
  are disposable and rebuildable. The conductor never reads Router internals;
  the Router never issues ledger SQL.
- **Deployment**: in-process binding first (typed module behind the contract);
  then a supervised process spoke over the existing IPC Connect envelope
  (`docs/ipc-connect-design.md`: `READY {json}`, `CommandSpec`, bind gate,
  capability negotiation). `AGALMA_ROUTER_TRANSPORT` selects the binding.
- **Replacement**: every recorded decision pins the Router binding
  id/version/generation; recovery never silently redirects. The process binding
  lands with dual-binding parity, staged rebind, and rollback evidence — the B1
  recipe at this boundary (SR.12).
- **Failure**: Router unavailable or failing closed → dispatch parks with a
  clear reason. The conductor never falls back to a compiled-in table except for
  an explicit `--model` override, recorded as `origin=override`.
- **Tier**: declared boundary (Tier 2 posture: canonical schemas, ownership
  declaration, `Describe`/`Health`, no private-state access) at introduction;
  Tier-1 ceremony applies to the process binding because routing sits on the
  dispatch path.

### 4.2 Registry (SR.1)

`Engine` enum (`opencode`, `codex`) and `RouteSpec` rows extending `ModelSpec`:
engine, provider, model id, capability (context, tool use, strict schema),
billing class (`free` | `subscription` | `cash`), quota bucket + window set
(unit: `usd` | `percent` | `credits`), price-schedule reference, data policy
(`training` | `zero-retention`), credential scope, provenance note. Rows are
reviewed static data owned by the Router; the registry carries a content revision
hash recorded in decisions. A model without a reviewed billing row is never
selectable.

### 4.3 Peak clock (SR.2)

`PriceSchedule` (UTC windows, weekday mask, static holiday exclusion list) and
`BillingClock::classify(spec, now) -> Free | OffPeak | Peak`. The clock is
injected; unit tests never read wall time.

### 4.4 Budget state (SR.3)

Durable accounting rebuilt from the ledger:

- `budget_observations` events: provider-reported consumed/limit/reset per
  bucket, with source (`opencode-usage` | `codex-rate-limits` | `local`);
- local usage events (already recorded) recomputed over rolling 5h/7d/30d
  windows from timestamps;
- reconciliation: a provider observation wins for `remaining`; local accounting
  is a lower bound when no observation exists;
- `BudgetSnapshot { bucket -> { limit, consumed, remaining, resets_at, source } }`
  is deterministic given ledger + clock; a passed reset restores the window.

### 4.5 Discovery (SR.4)

- OpenCode catalog snapshot (models.dev cache / catalog tool) with revision and
  `fetched_at`, diffed into reviewed registry rows.
- Codex: binary version + auth presence; plan declared by the operator
  (`AGALMA_CODEX_PLAN`, conservative default) until provider observations
  calibrate the windows.
- Credential detection stays parent-side and token-free (existing fingerprint
  audit pattern).
- Drift guard: a disappeared or changed model invalidates affected routes and
  records an event; re-promotion is a reviewed change.

### 4.6 Governor policy (SR.5)

Deterministic, pure, clock-injected policy exposed as `RouterApi::EvaluateRoute`:

1. Hard gates: engine available + credentialed, capability/context fit, data
   policy allows the task content, reviewed cost row, window headroom ≥ reserve
   (p95 attempt cost + floor).
2. Effective cost: `free = 0`; `subscription = shadow price λ(bucket)`;
   `cash = price × peak multiplier`. λ falls toward 0 when projected leftover at
   reset exceeds slack (use-it-or-lose-it) and rises toward blocking as
   remaining approaches reserve.
3. Order by effective cost, tie-break on the existing quality notes; deferrable
   work prefers off-peak windows (park until the window opens when the deadline
   allows).
4. Urgency override: near-deadline or interactive work may exceed the pacing
   target or pay peak rates; recorded as `urgency=high`.
5. Every decision records offered options with score and elimination reason, the
   budget snapshot hash, the clock, and the policy pin. The v1 static baseline
   remains the fallback when v2 has no eligible candidate or fails closed.

### 4.7 Engine selection per attempt (SR.6)

The route carries `engine`; harness binding selects the adapter per route
instead of the process-global kind. Recovery, rebind, and dual-binding rules are
unchanged; the digest records the engine.

### 4.8 Codex adapter (SR.7)

`agalma-harness-codex` implements `HarnessApi` over `codex exec --json`:
JSONL normalization (`turn.*`, token usage, rate-limit observations, `error`),
prompt via stdin, final message capture (`--output-last-message`), working
directory = isolated scratch checkout. OS confinement was removed by design
([sandbox-removal-design.md](sandbox-removal-design.md)); Agalma's own process
launcher is the boundary mechanism. Credentials stay parent-side; Codex
token refresh hosts go through the parent proxy allowlist, never into the
worker environment. Exact flags and stream shape are pinned from a captured fixture during
implementation (one gated live smoke); nothing is assumed from docs alone.

### 4.9 Observability (SR.8)

Agalma CLI commands become Router clients, not private-module readers:
`agalma models` (engine column, window state, pace, peak status, per-route skip
reasons via `Explain`) and `agalma budget` (per-bucket windows, consumption
source, resets). Digest gains engine, peak, and quota-window fields.

### 4.10 Failure recovery (SR.9)

Provider quota/rate errors (429, quota-exhausted) are classified, reported to the
Router as budget observations, the bucket is cooled down to its reset, and the
attempt is re-evaluated on the next eligible route without burning a semantic
attempt; a `route.fallback` + `provider_health` event is recorded.

### 4.11 Evidence (SR.10)

Deterministic scenario suite (fake ledger + injected clock): pacing,
use-it-or-lose-it, peak deferral, window reset, drift fail-closed, Codex adapter
fixtures, `RouterApi` conformance (replay, changed-input rejection). Process
binding evidence per the B1 recipe (dual-binding parity, staged rebind,
rollback). Gated live evidence in `docs/smart-routing-evidence.md`: an off-peak
shifted run, a budgeted v1-vs-v2 comparison batch, and a Codex smoke. Epic exit
is the metric table in §1.

## 5. Non-goals

- Quality evaluation, benchmark promotion, and auto-mutation of the routing
  table (M2 model-onboarding door owns that).
- Parallel dispatch, hedging, or multi-engine racing.
- Cash budget enforcement: spend remains report-only.
- Direct OpenAI/Anthropic/Google API integrations beyond what an engine already
  serves.
- Scraping holiday calendars; the exclusion list is static config.
- A new database per service: the Router stores durable facts through
  `LedgerApi` only.

## 6. Verification rules

- Every route decision carries the policy pin, Router binding pin, reasons,
  budget snapshot hash, and clock; `resolve_route` tests are extended, not
  replaced.
- `RouterApi` conformance: duplicate `EvaluateRoute` returns the recorded
  receipt; changed inputs under the same operation ID are rejected; unknown
  outcomes reconcile.
- Clock-dependent behavior is unit-tested with an injected clock only.
- A model without a reviewed billing row cannot be selected (registry test).
- Process binding: dual-binding parity, staged rebind, rollback evidence; the
  architecture test forbids impl→impl dependencies (conductor may not import
  `agalma-router` internals).
- Live spend runs only through the existing gated flags (`AGALMA_ALLOW_PAID`,
  `AGALMA_PAID_LIVE`), with artifacts under `target/test-runs/`.
