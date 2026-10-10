# S1 — stall root-cause: provider hang vs session-gone vs projection gap vs proxy

Bead: `agalma-csw.1` (S1). Epic: `agalma-csw` (M1.6). Base HEAD: `db7f081`
(the sibling `agalma-csw.2` commit `b461759` landed concurrently during this
spike; the code paths analysed here — `progress_fingerprint`/`poll_terminal`,
`close_session`/`cancel_operation` ordering, and the adapter `translate` — are
unchanged by it, so line refs below are against current HEAD `b461759`).
Date: 2026-10-10. Timeboxed spike; **no product-code changes** (a follow-up fix
bead is proposed, not implemented). Reproducer: `spikes/stall/` (+ `README.md`);
evidence under `spikes/stall/runs/` (gitignored).

Question: free Zen sessions "stall" with zero tokens, the idle watchdog cancels
at ~120 s, and the cancel returns `SessionNotFoundError`. Classify
(a) provider hang, (b) server-side session gone/crashed, (c) adapter projection
gap, (d) proxy/egress issue.

## Verdict

**(c) adapter projection gap.** The sessions are **not** stalled and the provider
is **not** hung: the model is actively producing tokens and running tool calls
throughout the whole idle window. The OpenCode adapter's progress fingerprint is
blind to a running turn (the durable session log emits only `log.synced`, and the
session projection's token counters lag/are ignored while `outcome` is absent),
so `poll_terminal` sees no change and fires the idle watchdog on a working
session. The batch's `SessionNotFoundError` is a **harness ordering artifact**
(`close_session` DELETE runs before `cancel_operation` interrupt), not a
server-side session loss.

| # | Hypothesis | Verdict | One-line basis |
|---|-----------|---------|----------------|
| a | provider hang | **no** | server logs show model shell-tool calls at 10/86/98/119 s in attempt 1 and through the window in every attempt; token counters populate on step completion |
| b | server-side session gone/crashed | **no** | 0 `ERROR`/`WARN`/panic lines; `/api/session/{id}` stays 200; the 404 reproduces by DELETE-then-interrupt and disappears on interrupt-before-close |
| c | adapter projection gap | **YES** | fingerprint constant for 130 s while the session completes 4 tool steps; the durable log emits only `log.synced`; live progress exists only on SSE (unparsed) |
| d | proxy/egress issue | **no** | every attempt reached the provider (tokens + tool calls); the proxy allowlists `opencode.ai`; no transport errors |

## Method

1. Mined the recorded H4 batch (`target/test-runs/hardening-batch/run-1/`):
   `batch.log`, per-attempt `xdg-data/opencode/log/opencode.log`, the isolated
   OpenCode SQLite DB, the ledger (`state/ledger.sqlite` -> `execution_events`).
2. Read the conductor/adapter code that produces the behaviour
   (`crates/agalma-conductor/src/phases.rs` `poll_terminal`/`progress_fingerprint`,
   `crates/agalma-harness-opencode/src/{adapter,translate}.rs`).
3. Reproduced with a plain local `opencode serve` (`spikes/stall/repro.sh`):
   isolated XDG, create session, prompt a multi-step tool turn, and poll
   `inspect_operation`-equivalent state, the durable log, the live message list,
   SSE, and server liveness while the harness's cancel order is replayed.
4. Cross-checked against `mimo-v2.6-flash-free` behaviour by design of the same
   flow (see Open items).

## Evidence

### (a) Provider is responsive — not hung

Attempt 1 of task `agalma-9u8` (`…/exec-agalma-9u8-1/xdg-data/opencode/log/opencode.log`,
`run=a9643b57`, server `127.0.0.1:53026`) shows the agent's shell tool executing
through the entire 120 s window, right up to the interrupt:

```
03:56:39 spawn process /bin/zsh -c "cargo test"
03:57:55 /bin/zsh -c "ls -la && cat .cargo/config.toml 2>/dev/null; ls crates"
03:58:07 /bin/zsh -c "ls docs/ && test -f docs/conventions.md && echo EXISTS"
03:58:29 /bin/zsh -c "git log --oneline -5 && git status --short"   # 119.4 s in
03:58:31 interrupt -> 404 SessionNotFoundError                       # 120.9 s in
```

The same pattern holds for `cvr`, `hhn`, and `rxb` attempt 1 (agent `/bin/zsh`
calls up to the interrupt). The isolated DB for `9u8` (recovered from orphaned
pages with `sqlite3 .recover`) holds assistant messages with real usage and tool
content:

```
"tokens":{"input":437,"output":335,...}   "type":"tool" (×7)   "outcome":"interrupted"
```

The ledger `provider_health` events (`state/ledger.sqlite`) record `tokens:0`
for all 7 stalls — because the adapter never emitted a `UsageSnapshot` while
running (see (c)), **not** because the provider returned nothing.

The one productive session, `rxb` attempt 2, recorded `tokens_in=23003
tokens_out=1071` with `wall_ms=115752` — i.e. it reached a terminal `succeeded`
outcome **~4.2 s inside** the 120 s idle bound. Sessions slower than the bound are
killed regardless of progress.

### (b) Server stayed up; the session was not gone

- No `level=ERROR`, `level=WARN`, panic, or fatal line in any attempt's
  `opencode.log`; each server booted, served, and shut down cleanly after the
  harness's SIGTERM (`watcher stopped`).
- `batch.log` shows one fresh server per attempt (ports 53026, 53821, 54597,
  … , 58551) — expected attempt-scoped launches, not a crash/restart of a shared
  server.
- Live repro: `GET /api/session/{id}` returns 200 for the whole run; interrupting
  a live session returns `{"interrupted":true}` / http 200. Reproducing the
  harness's order — `DELETE` (`close_session`, http 204) **then** interrupt —
  returns exactly the batch error:

  ```
  POST /api/session/{sid}/interrupt
  {"_tag":"SessionNotFoundError","sessionID":"ses_…","message":"Session not found: ses_…"}  [404]
  ```

  Cause is in `crates/agalma-conductor/src/phases.rs` `drive()`: `close_session`
  (line 710) runs before the `PollOutcome::Stalled` arm's `cancel_operation`
  (line 743). The session is deleted by the harness itself, then cannot be
  interrupted. The batch's `SessionNotFoundError` is therefore a **red herring**.

### (c) Projection gap — the root cause

`poll_terminal` advances the idle clock only when `progress_fingerprint` changes
(`crates/agalma-conductor/src/phases.rs:2542`). The fingerprint is
`{state, tokens_in/out, cost, steps}` where tokens come from `Event::UsageSnapshot`
and `steps = read_events(op).len()`.

`translate::events_from` (`crates/agalma-harness-opencode/src/translate.rs:75`)
emits `TurnStarted`, then a `UsageSnapshot` **only** when the session projection
is terminal (`succeeded`/failed). While Running it adds nothing (and the durable
log contributes nothing, see below). So for a running turn the fingerprint is
constant `{Running, 0, 0, 0, steps:1}` — `read_events` returns just
`[TurnStarted]`.

Reproducer output (full run, `spikes/stall/runs/stall-20261010T133042Z`), a
4-step shell sequence against `opencode/big-pickle`, idle bound 130 s:

```
t+0s   alive=yes [state=Running steps=1] tokens=0/0       log_seq=4  tool_state=none       | SSE steps=0 tools=0 reasoning=0
t+5s   alive=yes [state=Running steps=1] tokens=0/0       log_seq=13 tool_state=running    | SSE steps=1 tools=1 reasoning=4
t+40s  alive=yes [state=Running steps=1] tokens=6151/61   log_seq=22 tool_state=completed,running | SSE steps=2 tools=2 reasoning=4
t+75s  alive=yes [state=Running steps=1] tokens=6311/114  log_seq=31 tool_state=completed,running | SSE steps=3 tools=3 reasoning=4
t+110s alive=yes [state=Running steps=1] tokens=6412/167  log_seq=40 tool_state=completed,running | SSE steps=4 tools=4 reasoning=4
t+130s alive=yes [state=Running steps=1] tokens=6412/167  log_seq=40 tool_state=completed,running | SSE steps=4 tools=4 reasoning=4

# distinct fingerprints observed while running:
#   state=Running steps=1
```

Interpretation:

- The model issued **4 tool calls that all completed** and accumulated real
  tokens (projection `tokens` 0 -> 6151 -> 6311 -> 6412); the last step finished
  ~20 s before the idle bound.
- The adapter fingerprint never changed — a stall verdict at 120 s is a **false
  positive**.
- `GET /api/experimental/session/{id}/log` emitted only
  `data: {"type":"log.synced","aggregateID":"…","seq":N}` (no tool/step frames),
  matching S0a/S0d. `seq` advances but carries no event rows (`event` table 0
  rows).
- Live progress *is* available — just not on the paths the adapter reads:
  `GET /api/event` (SSE) streamed `session.step.started`, `session.tool.called`,
  `session.tool.progress`, `session.reasoning.delta`, `session.execution.started`;
  `GET /api/session/{id}/message` reflected live part state
  (`"type":"tool","state":{"status":"running"}`).
- The projection's own token counters *do* update on step completion, but
  `events_from` ignores them unless the turn is terminal, and the fingerprint
  reads only `UsageSnapshot` events.

### (d) Egress fine

Every attempt reached the provider (tokens in the DB and tool calls executed).
The batch proxy allowlist is `opencode.ai` (`provider_proxy.rs` `DEFAULT_ALLOW`)
and the model catalog source `models.opencode.ai` is a subdomain (also allowed);
no `provider-proxy: deny` was produced. The proxy does not persist per-request
counters unless `PROVIDER_PROXY_LOG`-style logging is enabled, so (d) is excluded
by the positive provider evidence above, not by counters. A dead-egress control
(plain server with `HTTPS_PROXY` at a closed port) fails the *catalog* fetch
immediately and the *provider* call fails fast — it does **not** present as a
120 s silent stall, so a proxy/egress failure is not the mechanism.

## Root cause chain

1. Free Zen models often take > 120 s per turn (many tool steps; `rxb` attempt 2
   was 115.75 s, barely inside the bound).
2. The adapter's `read_events` yields no progress for a running turn: the durable
   log has only `log.synced`, and projection usage is emitted only at terminal.
3. `progress_fingerprint` is therefore constant -> the idle watchdog declares
   `idle` at 120 s -> `stalled` verdict -> retry/park.
4. `drive()` deletes the session (`close_session`) before cancelling it, so the
   post-stall interrupt always 404s — the misleading `SessionNotFoundError`.

## Reproducer

```
# permanent, under spikes/stall/ ; runs/ is gitignored
sh spikes/stall/repro.sh                 # ~140 s, idle bound 130 s
QUICK=1 sh spikes/stall/repro.sh         # ~15 s smoke
```

Observed (both `QUICK=1` and full), matching the four buckets:

```
# server : pid=… port=45162 version=2.0.15 ready=1 boot_attempts=1
# session: ses_… (prompt http=200 after 0 create attempts)
t+0s   alive=yes [state=Running steps=1] tokens=0/0    log_seq=4  …
t+5s   alive=yes [state=Running steps=1] tokens=0/0    log_seq=13 … SSE steps=1 tools=1 reasoning=4
t+110s alive=yes [state=Running steps=1] tokens=6412/167 … SSE steps=4 tools=4 reasoning=4
# distinct fingerprints observed while running:
#   state=Running steps=1
# --- harness cancel ordering (close_session DELETE, then cancel_operation interrupt) ---
# GET session before cancel:  200
# DELETE (close_session):     204
# POST interrupt (cancel):    {…"SessionNotFoundError"…} [404]
# control GET live session:   200
# control interrupt-before-close: {"interrupted":false} [200]
# server alive after teardown: no
```

Environment notes (see `spikes/stall/README.md`): the script pins the v2.0.15
binary (`/opt/homebrew/bin/opencode` is v1.18.25 and does not speak the v2 API)
and sets `OPENCODE_MODELS_URL=http://127.0.0.1:1` so a cold boot does not hang on
the `models.opencode.ai` catalog fetch.

## Minimal fix proposal (adapter-side; follow-up bead)

The seam has no progress signal for a running turn, so the watchdog cannot tell
"working" from "hung". Smallest change: make the adapter surface live progress.

- `translate::events_from` / `read_events`: while the projection is non-terminal,
  emit a best-effort partial `UsageSnapshot` from the projection's `tokens`
  (it updates on step completion) — a changing token count resets the idle
  clock. This is a few lines and needs no new endpoint.
- Optionally also count live parts from `GET /api/session/{id}/message`
  (or drains of `/api/event` on a background reader) so a turn that is only
  streaming reasoning also resets the clock. SSE is volatile, acceptable for a
  *liveness* signal (a dropped frame just falls back to the idle bound); it is
  not used for reconciliation.
- Independent, tiny conductor fix: on the `Stalled`/`TimedOut` paths call
  `cancel_operation` **before** `close_session` (or skip the cancel and rely on
  `stop_attempt`), so the interrupt is not a guaranteed 404 and the
  `SessionNotFoundError` on stalls stops being misleading.

Do **not** raise the idle bound as the primary fix: it only trades false-stalls
for longer false-stalls; the watchdog already waits the correct wall time, it
just cannot see the progress.

## Deviations / open items

- Timebox spent mostly on environment flakiness (binary shadowing, cold-boot
  catalog fetch); the classification itself was settled from the recorded batch.
- `mimo-v2.6-flash-free` was not re-run live within the timebox; the recorded
  M1.8 run shows the same zero-token stall signature, and the projection gap is
  model-independent (it is a property of the adapter), so the verdict holds.
- No product code changed; no git commit/push; bd read-only.
- The batch did not persist proxy CONNECT counters (logging off by default), so
  (d) rests on provider-reachability evidence rather than counters.
