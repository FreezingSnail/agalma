# TUI control plane — design (read model, live watch, terminal stack, control policy)

Bead: `agalma-3kr.1` (TUI-D1). Epic: `agalma-3kr`. Orchestrator-authored; gates
TUI children. Human-approved terminal stack: **ratatui + crossterm**
(2026-10-11; recorded in `docs/m0-dependencies.md`). The CP.6 pilot
(`agalma-i5h.7`) may re-decompose children through the pipeline; this document
is the source of truth for that decomposition, the hand-authored children are
reference input only.

Goal: a human-operated control plane with full visibility into the factory —
executions/phases/attempts, component bindings and transports, economics, and
gated control actions — riding existing APIs only (ledger, component sockets).

## Read model

`agalma status --json` is the single read model; the TUI never opens the ledger
itself, it consumes this snapshot (in-process call, or `--json` parse).

Schema (stable; additive changes only, bump `schema_version` field to match):

```json
{
  "state_dir": "<path>",
  "exists": false,
  "kill_latch": false,
  "schema_version": 4,
  "binding": "reference@reference/v1",
  "executions": [
    {
      "execution_id": "exec-...",
      "phase": "Build",
      "state": "Running",
      "attempt": 2,
      "pending_operations": ["op-..."],
      "parked_reason": null,
      "harness": "reference@reference/v1"
    }
  ]
}
```

- `exists:false` + empty list + `schema_version:null` + `binding:null` when no
  ledger is present; exit 0. Read-only; conductor crate only; std + existing
  deps only. Extra fields (`parked_reason`, per-execution `harness`) stay.
- Sources: `LedgerApi::schema_version()`, `pending_intents()` per execution,
  `harness_binding::latest_binding()` for the top-level label.
- Landed already: partial snapshot via factory run `exec:agalma-wnm`
  (`42c4a8e`). Completion tracked with the fidelity gap (`agalma-3j1`) under
  TUI.1.

## Live transport

Polling, not push (v1). `agalma-ipc` is request/response today (4-byte BE length
+ JSON envelope, no subscriptions, one in-flight call per connection); the
conductor's own component sockets follow the same shape. The IPC v2 design
(`docs/ipc-connect-design.md`) replaces the framing with ConnectRPC
(multiplexed calls, future server-streaming option), but v1 watch stays a
read-only refresh loop so no component changes are needed.

- `agalma watch` (new conductor subcommand): full-screen refresh loop calling
  the same in-process read-model function as `status --json`; default 1 s
  interval (`--interval-ms`), `q`/Ctrl-C exits, SIGWINCH resize, no writes.
- `status --json` currently opens `SqliteLedger` directly, bypassing
  `AGALMA_LEDGER_TRANSPORT` / `LedgerHandle`. Decision: keep direct read for
  v1 (state dir is single-writer; needs WAL confirmation — see Risks); switch
  to `LedgerHandle` only if concurrent-reader verification fails.
- No new daemon, no new socket. Server-side streams (SSE etc.) are a non-goal.

## Terminal stack

- `ratatui = 0.30.2`, `crossterm = 0.29.0`, both pinned `=` in the workspace
  `[workspace.dependencies]`; default ratatui backend (crossterm). User
  approved 2026-10-11 under the `docs/m0-dependencies.md` policy (`cargo
  update` stays a reviewed change).
- Implementation lives in the conductor crate (`crates/agalma-conductor/src/`
  `tui/` module + `watch` subcommand) — one binary, no new workspace member;
  ratatui/crossterm are compile-time deps of `agalma-conductor` only.
- All rendering through ratatui `Terminal`/`Frame`; raw-mode guard restores
  the terminal on all exit paths (panic hook included). Std-only ANSI fallback
  is dropped now that the stack is approved.
- Tests: render-level tests against `ratatui::TestBackend` (no TTY); interval
  loop tested with an injected snapshot provider.

## Control policy

Actions are human-only; the TUI adds no privileges the CLI does not have.

- **Pause** = set the kill latch (stops new claims/dispatch; in-flight worker
  is not interrupted). **Resume** = clear latch + remove `STOP` sentinel —
  same semantics as `agalma resume`, plus a typed confirmation
  (`type "resume"`).
- **Cancel execution** = `ExecutionApi::cancel` for the selected execution;
  typed confirmation; no silent Git/DB rewinds, no schema rewinds.
- **Receipts** (every action, durable): ledger schema **v4** adds
  `control_receipts(action_id PK, action, target, actor, request_json,
  recorded_at)`, additive migration; `LedgerApi` gains
  `record_control_receipt` / `control_receipts`, exposed over IPC like every
  other ledger method. Execution-targeted cancels additionally write the
  existing `cancel_requested` event. Failed/policy-denied attempts are
  receipted too (outcome field in `request_json`).
- Pause/resume receipts show in the timeline (TUI.4) and gate nothing.

## Increments

| Child | Scope | Depends on |
|---|---|---|
| TUI.1 `agalma-3kr.2` | read-model completion: `schema_version`, `pending_operations`, top-level `binding`; grep-level acceptance; no TTY tests | — |
| TUI.2 `agalma-3kr.3` | `agalma watch`: ratatui shell, executions table (phase/state/attempt/pending ops, kill latch, binding), refresh loop, resize/quit | TUI.1 |
| TUI.3 `agalma-3kr.4` | component vision: latest + per-op pinned bindings, binding states, transports (`in-process|process` envs), sandbox `alive` / server pid+socket / `provider_health` liveness | TUI.1 |
| TUI.4 `agalma-3kr.5` | economics + timeline: derived spend (`usage` events / `CostSummary`), `budget_usd` from intake fence (display only), digests, parked/degradation alerts, curated `execution_events` timeline | TUI.1 |
| TUI.5 `agalma-3kr.6` | controls: pause/resume/cancel, typed confirmations, ledger v4 `control_receipts`, cancellation events | TUI.1 |
| TUI.6 `agalma-3kr.7` | full-screen composition: panes, keybindings, focus model, `docs/tui.md`; pane resize isolation | TUI.2–TUI.5 |

Each child keeps the fixed bd description headings + `# MACHINE` block per
`docs/planning-pipeline.md` when re-decomposed by the pilot.

## Non-goals

- No push/stream transport (poll only); no SSE consumption.
- No `Health` type in v1 (does not exist; liveness proxies only) — future bead.
- No budget enforcement (none exists); economics is display-only.
- No remote/multi-user access, no auth model (single local human operator).
- No writes beyond latch/STOP, cancel, and control receipts.

## Risks / open items

- Ledger journal mode not verified for concurrent readers while the factory
  writes; TUI.2 verifies WAL before shipping `watch`.
- Ledger v4 bump touches `LEDGER_METHODS` surface (IPC) and migration tests;
  scoped to the additive table only.
- ratatui 0.30.2 pulls new transitive crates (ratatui-core, ratatui-widgets);
  stays inside the approved direct pins.
- `latest_binding` is a global label; per-execution/per-operation pins come
  from `harness_binding` events — TUI.3 maps them explicitly.
- Decisions have no per-execution list API (only `decision_outcome`);
  timeline v1 uses events + digests, decisions column is future work.
