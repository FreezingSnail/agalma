# S0a — harness spike runbook

Task: agalma-a49. Status: ready. Timeboxed; kill/fallback decision at exit.
Architecture refs: `docs/architecture.md` §13 S0a, §3.2.

Prove the OpenCode v2 surface Agalma intends to drive, and correct the
architecture where it is wrong. Discovery run, not product code. Any code,
scripts, or profiles written land permanently under `spikes/s0a/` — none in
temporary directories.

## Exit criteria

- [ ] spawn `serve`; record auth model (v2.0.15 `serve --help` shows no password flag)
- [ ] transport verdict: localhost HTTP/SSE vs `serve --stdio`
- [ ] `GET /api/info` captured
- [ ] session created
- [ ] prompt pinned cheap model; SSE read to completion; event names and shapes captured
- [ ] independent agalma checkout at pinned SHA; one command run inside session; effect verified in checkout
- [ ] `tool.execute.before` verdict: can block, or mutate-only
- [ ] `POST /api/location/reload` verified
- [ ] stats/pricing surface captured (`/api/experimental/session/stats`, `/api/model`)
- [ ] §3.2 endpoint list marked verified or corrected
- [ ] user OpenCode state untouched
- [ ] results recorded below; new `[open]` items listed

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

### Exit criteria

| Item | Status | Evidence |
|---|---|---|
| serve spawn + auth | | |
| transport verdict | | |
| `/api/info` | | |
| session create | | |
| prompt + SSE | | |
| checkout + in-session command | | |
| `execute.before` block? | | |
| reload | | |
| stats/pricing | | |
| endpoint list | | |
| user state untouched | | |

### Endpoint observations

| §3.2 candidate | Observed | Verdict |
|---|---|---|
| `GET /api/info` | | |
| `GET /api/model` | | |
| `GET /api/provider` | | |
| `GET /api/agent` | | |
| `POST /api/session` | | |
| `POST /api/session/{id}/prompt` | | |
| `POST /api/session/{id}/wait` | | |
| `POST /api/session/{id}/interrupt` | | |
| `POST /api/session/{id}/model` | | |
| `POST /api/session/{id}/agent` | | |
| `POST /api/session/{id}/synthetic` | | |
| `POST /api/session/{id}/compact` | | |
| `GET /api/session/{id}/diff` | | |
| `GET /api/experimental/session/stats` | | |
| `POST /api/location/reload` | | |
| `/api/worktree` CRUD | | |
| `GET /api/event` (SSE) | | |

### Deviations and new open items

- ...

### Verdict

- proceed / fallback / kill:
