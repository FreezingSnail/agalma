# S0a — harness spike runbook

Task: agalma-a49. Status: done (verdict: proceed). Timeboxed; kill/fallback decision at exit.
Architecture refs: `docs/architecture.md` §13 S0a, §3.2.

Prove the OpenCode v2 surface Agalma intends to drive, and correct the
architecture where it is wrong. Discovery run, not product code. Any code,
scripts, or profiles written land permanently under `spikes/s0a/` — none in
temporary directories.

## Exit criteria

- [x] spawn `serve`; record auth model (v2.0.15 `serve --help` shows no password flag)
- [x] transport verdict: localhost HTTP/SSE vs `serve --stdio`
- [x] `GET /api/info` captured
- [x] session created
- [x] prompt pinned cheap model; SSE read to completion; event names and shapes captured
- [x] independent agalma checkout at pinned SHA; one command run inside session; effect verified in checkout
- [x] `tool.execute.before` verdict: can block, or mutate-only
- [x] `POST /api/location/reload` verified
- [x] stats/pricing surface captured (`/api/experimental/session/stats`, `/api/model`)
- [x] §3.2 endpoint list marked verified or corrected
- [x] user OpenCode state untouched
- [x] results recorded below; new `[open]` items listed

Kill/fallback: if serve cannot be driven headless or SSE is not consumable,
fall back to `--stdio` transport. If neither works, kill and report — the M0
harness plan is invalid.

## Isolation

- Fresh `XDG_CONFIG_HOME` / `XDG_DATA_HOME` per run: `spikes/s0a/runs/<UTC>/xcfg`, `.../xdata`.
- Provider credential: copy `~/.local/share/opencode/auth.json` into the
  isolated data dir, chmod 600, never commit. Alternative: provider env var.
  Record which path worked.
- Never mutate `~/.config/opencode`, `~/.local/share/opencode`, or the user's
  working checkout.

## Procedure

0. Record environment: opencode version, macOS version, base SHA (HEAD of this
   run; record the actual SHA in results).
1. Start `opencode serve --hostname 127.0.0.1 --port 39001`; capture
   stdout/stderr into the run dir; record the auth handshake.
2. Probe `GET /api/info`, `/api/model`, `/api/provider`, `/api/agent`; save
   redacted JSON.
3. Pin a model: choose the cheapest capable entry from `/api/model`; record id
   and pricing fields.
4. Checkout: `git clone /Users/connorfranc/code/agalma <run>/checkout`, then
   `git -C <run>/checkout checkout <base SHA>`.
5. Session: `POST /api/session`; `POST /api/session/{id}/prompt`; read
   `GET /api/event` (SSE) to turn completion. Capture turn events, tool events,
   usage snapshots.
6. Command-in-session: prompt the agent to run `uname -a` and write a
   `S0A_MARKER` file in the checkout; verify the file and record the tool event.
7. `execute.before` test: isolated-config plugin throws and writes a marker on
   invocation; observe block vs mutate. Repeat with a return-value mutation.
   Record exact behavior and hook payload shape.
8. Reload: modify an isolated agent config, `POST /api/location/reload`, verify
   the change took effect.
9. Worktree CRUD: exercise `/api/worktree`; record request/response shapes.
10. Teardown: SIGTERM the server; confirm no orphan `opencode` processes.
    Compare user-state mtimes before/after (must be unchanged). Measure idle
    RSS/CPU while serving.

## Recording

- Raw redacted captures and logs: `spikes/s0a/runs/<UTC>/` (gitignored).
- Summary, payload shapes, and verdicts: this file. Redact tokens and
  passwords everywhere.

## Results

Run: `spikes/s0a/runs/20261009T170125Z/` (gitignored; raw redacted captures in `capture/`,
`server*.out|err`, `sse*-events.txt`, `plugin-hook.log`, `user-state-*.txt`).

Environment: opencode **v2.0.15** (source sha `6f3639d82ed0760091792189b78f8eeb44f699b1`,
`release: v2.0.15`); macOS 26.6.2 (25G83), arm64; agalma base SHA
`8673827e7fc313fe8afbfcf2a4ed3eca712319e5`; isolated `XDG_{CONFIG,DATA,CACHE,STATE}_HOME`
plus `TMPDIR` under the run dir; server `http://127.0.0.1:39001`.

### Exit criteria

| Item | Status | Evidence |
|---|---|---|
| serve spawn + auth | ✅ | `server listening on http://127.0.0.1:39001`; unauth `/api/info` → `401` + `www-authenticate: Basic realm="Secure Area"`; `-u opencode:$OPENCODE_PASSWORD` → `200`. No `--password` flag; password from `OPENCODE_PASSWORD` (else random, printed to stdout only in foreground). |
| transport verdict | ✅ | Localhost HTTP/SSE fully driven headless. `serve --stdio` smoke: prints `{"url":"http://127.0.0.1:39002"}` and serves HTTP, exits on stdin close → it is a lifecycle mode, not a second wire protocol (see Deviations). |
| `/api/info` | ✅ | `{"version":"2.0.15","pid":<pid>,"urls":["http://127.0.0.1:39001"],"paths":{"tmp":"…/run/tmp/opencode"}}` (tmp honors isolated `TMPDIR`). |
| session create | ✅ | `POST /api/session` → `{"data":{id,projectID,model,cost,tokens,time,title,location}}`, `model.variant` defaults `"default"`. |
| prompt + SSE | ✅ | Pinned `opencode/mimo-v2.6-flash-free` (cost 0). `POST …/prompt` → `{"data":{inbox user…}}` (durable admit, returns before the turn). SSE read to completion in 3.1 s; `POST /api/experimental/session/{id}/wait` → `204`. Event sequence: `server.connected`, catalog `*.updated`, `session.inbox.enqueued`→`execution.started`→`step.started`→`reasoning.started/delta/ended`→`text.started/delta/ended`→`step.streamed`→`step.ended`→`usage.updated`→`execution.succeeded`. |
| checkout + in-session command | ✅ | `git clone` + `checkout 8673827…` in `run/checkout`. Agent bash/shell tool ran `printf S0A_MARKER > S0A_MARKER.txt && uname -a >> …`; file verified (138 B: `S0A_MARKERDarwin Mac 25.6.0 … arm64`). Events `session.tool.called/input.started/input.ended/progress/success`, `shell.created/exited`. Also `POST …/shell` (no model) ran `uname -s && printf S0A_SHELL > S0A_SHELL.txt`, verified. |
| `execute.before` block? | ✅ **Block AND mutate** | Mutate: hook rewrote `event.input.command`; executed `S0A_MUTATED.txt` created, original `S0A_ORIGINAL.txt` absent. Block: hook threw → `session.tool.failed {"error":{"type":"unknown","message":"S0A_BLOCK"}}`, `S0A_BLOCKFILE.txt` not created, run still `execution.succeeded`. Both plugin (promise) and effect APIs can mutate `event.input` and fail the hook (`Tool.Error`) before execution. |
| reload | ✅ | Added isolated `agent/s0a-test.md` → `POST /api/location/reload` `204` → `/api/agent` list changed from 7 to 8, `GET /api/agent/s0a-test` returns it. Adding `plugin/s0a-probe.js` likewise loaded `state:"active"` after reload. |
| stats/pricing | ✅ | `GET /api/experimental/session/stats` → `{"data":[range,sessions,subagents,prompts,steps,tokens,cost,tools,activeDays,streak,activity,models]}`. Pricing in `/api/model` `cost[] = {input,output,cache:{read,write}}` + `limit{context,output}`. `session.step.ended` carries `cost`,`tokens`. |
| endpoint list | ✅ | §3.2 subset corrected (below); full v2.0.15 surface = 113 paths in `packages/protocol/openapi.json`. |
| user state untouched | ✅ | `stat` mtimes of `~/.config/opencode`, `~/.local/share/opencode`, `opencode.jsonc`, `cli.json` byte-identical before/after; no writes under user XDG; server SIGTERM clean, no orphan; user's pre-existing `opencode` pid untouched. Idle RSS ≈ **456 MB**, CPU 0.8–1.8%, VSZ ≈ 441 GB (reserved). |

### Endpoint observations

| §3.2 candidate | Observed | Verdict |
|---|---|---|
| `GET /api/info` | 200 `{version,pid,urls,paths.tmp}`; replaces former health/server | ✅ keep |
| `GET /api/model` | 200 `{location,data:[Model.Info]}`, each with `cost[]`,`limit`,`capabilities`,`variants` | ✅ keep |
| `GET /api/provider` | 200 `{location,data:[]}` in isolation. Availability gated on integration connection + loaded records; free zen provider still usable without it | ⚠️ shape ok, availability gotcha |
| `GET /api/agent` | 200 `{location,data:[{id,…}]}`; 7 built-ins (`build,general,explore,compaction,title,summary,plan`) + config agents | ✅ keep |
| `POST /api/session` | 200 `{data:Session.Info}` (wrapped) | ✅ keep |
| `POST /api/session/{id}/prompt` | 200 `{data:Inbox.User}`; durable admit; supports `delivery`,`resume` | ✅ keep |
| `POST /api/session/{id}/wait` | **404** — endpoint lives at `/api/experimental/session/{id}/wait` | ❌ corrected |
| `POST /api/session/{id}/interrupt` | 200 `{interrupted:bool}`; query `resume` (was `continue`) | ✅ corrected param |
| `POST /api/session/{id}/model` | 204 body `{model:Model.Ref}` | ✅ keep |
| `POST /api/session/{id}/agent` | 204 body `{agent:Agent.ID}` | ✅ keep |
| `POST /api/session/{id}/synthetic` | 200 `{data:InboxSynthetic}`; `resume:false` admits without running | ✅ keep |
| `POST /api/session/{id}/compact` | 200 `{data:InboxCompaction}`; `delivery` steer/queue | ✅ keep |
| `GET /api/session/{id}/diff` | 200 `{data:FileDiff[]}` (empty here) | ✅ keep |
| `GET /api/experimental/session/stats` | 200 `{data:SessionStats.Info}`; experimental-only | ✅ keep (exp) |
| `POST /api/location/reload` | 204; rebuilds all locations; running sessions continue at next step boundary | ✅ keep |
| `/api/worktree` CRUD | GET `?projectID=` → bare `[Worktree.Directory]`; POST body `{projectID,name?}` → `{directory}`; DELETE body `{projectID,directory,force}` → 204; POST `/api/worktree/refresh` → 204. No `/api/worktree/{id}` path | ✅ corrected |
| `GET /api/event` (SSE) | 200 `text/event-stream`; frames `data: {json}\n\n`, `: heartbeat\n\n` @15 s; volatile: 4096-slot dropping queue, overflow fails stream, disconnects miss events | ✅ keep, note contract |

New v2.0.15 endpoints not in §3.2 but relevant to Agalma: `POST /api/session/{id}/shell`
(deterministic command, no model; events `shell.created`,`session.shell.started/ended`),
`POST /api/session/{id}/background`, `POST /api/session/{id}/move`,
`…/revert/{stage,commit}` + `DELETE …/revert`, `GET /api/experimental/session/{id}/log?follow=true`
(durable session event log for reconciliation), `/api/permission/*`,
`/api/session/{id}/form/*`, `/api/config/shell`, `/api/vcs/*`, `/api/pty`, `/api/shell`,
`/api/fs/{read,list,find}`, `/api/command`, `/api/skill`, `/api/mcp`, `/api/websearch`,
`/api/rpc/{rpcID}/{method}`.

### Deviations and new open items

- **`wait` path wrong in §3.2**: it is `POST /api/experimental/session/{id}/wait` (`experimental.session.wait`), not `/api/session/{id}/wait` (404). Update §3.2.
- **`/api/worktree` is not id-CRUD**: project-scoped list/create/remove(+refresh), DELETE carries a body; no `/api/worktree/{id}`. Update §3.2 wording.
- **`serve --stdio` is not a transport alternative**: it still serves loopback HTTP and prints `{"url":…}`; stdin close shuts it down. The kill/fallback fallback should be stated as "embedded lifecycle", not "different wire protocol". Password is *not* printed in stdio mode, so callers must set `OPENCODE_PASSWORD` or fetch the service credential.
- **Server auth model**: Basic, username `opencode`, password via `OPENCODE_PASSWORD` or auto-generated; `?auth_token=<base64(user:pass)>` accepted (browser/WS path). No `--password` flag in v2.0.15. `--service` persists its own password in `service.json` under the config dir.
- **Provider credential isolation gotcha**: the legacy `auth.json` import migration reads `<data>/auth.json` (`$XDG_DATA_HOME/opencode/auth.json`), but on a fresh isolated DB it inserted no `credential` row and `/api/provider` returned `[]`. Setting `OPENCODE_API_KEY` created an integration connection (`{"type":"env","name":"OPENCODE_API_KEY"}` on `opencode` and `opencode-go`) yet `/api/provider` stayed `[]`. Prompting still works on the built-in free `opencode` provider (`apiKey:"public"`, no connection). Agalma must not assume `/api/provider` reflects auth.
- **SSE is explicitly volatile** (overflow/disconnect loses events). Agalma reconciliation must use `…/experimental/session/{id}/log?follow=true` (durable, seq-addressable) or the idle barrier `…/wait`, never assume SSE continuity. [open]
- **`execute.before` resolved**: it can both mutate `event.input` in place and block by failing the hook; `session.tool.called` emits the *pre-hook* input, execution uses the post-hook input. §3.2 `[open]` can be closed. [open] → resolved
- **Default shell is the user login shell** (`/opt/homebrew/bin/fish` here; model tool run used `/bin/zsh`). Adapter must pin shell via `/api/config/shell`. [open]
- **Model catalog in isolation** listed only 7 free `opencode` zen models; `opencode-go` models were not present. Confirm the intended provider/model wiring for the pinned genome. [open]
- **`session.tool.called` `executed:false`** on local tool events; clarify semantics for accounting (likely provider-executed flag). [open]
- Idle server RSS ≈ 456 MB per Location — material for S0b/S0d resource budgets. [open]

### Verdict

- **proceed** — localhost HTTP + SSE drives every required scenario headless on v2.0.15;
  server spawn/auth, session/turn lifecycle, tool + shell execution in an independent
  checkout, `execute.before` block/mutate, reload, stats/pricing, and worktree CRUD all
  verified; user OpenCode state untouched. Adopt the corrected endpoint list and the
  durable session log (not SSE) for reconciliation.
