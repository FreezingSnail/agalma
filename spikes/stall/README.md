# spikes/stall — stall root-cause reproducer

Permanent reproducer for bead `agalma-csw.1` (S1 stall root-cause spike).
Verdict and full evidence: `docs/spikes/stall-root-cause.md`.

## What it shows

A free-Zen session that is demonstrably **working** (the model issues several
sequential shell tool calls, each completing) is nevertheless invisible to the
OpenCode adapter's progress watchdog:

- `GET /api/session/{id}` reports `outcome` absent and `tokens` stale while the
  turn runs (token counters only advance when a step completes).
- `GET /api/experimental/session/{id}/log` emits **only** `log.synced` — no
  tool/step frames.
- The adapter's `ProgressFingerprint` (`read_events` -> `TurnStarted` plus a
  `UsageSnapshot` only at terminal) therefore never changes while Running, so
  `poll_terminal` declares `idle` at the idle bound even though SSE
  (`/api/event`) is streaming live `session.step.*` / `session.tool.*` /
  `session.reasoning.*` frames that the adapter ignores.

It also reproduces the harness cancel path: the conductor calls
`close_session` (DELETE) **before** `cancel_operation` (interrupt), so the
interrupt returns `SessionNotFoundError` (404) even though the server and the
session were alive — the 404 is a harness ordering artifact, not a server-side
session loss. A control interrupt on a live session returns 200.

## Run

```
sh spikes/stall/repro.sh                 # ~140 s: multi-step turn > 130 s idle bound
IDLE_SECS=130 sh spikes/stall/repro.sh   # explicit idle bound
QUICK=1 sh spikes/stall/repro.sh         # ~15 s smoke
MODEL=opencode/mimo-v2.6-flash-free sh spikes/stall/repro.sh
```

Env knobs: `MODEL`, `IDLE_SECS`, `STEPS`, `STEP_SLEEP`, `POLL`, `PORT`,
`OPENCODE_BIN`. Evidence (gitignored) lands in `runs/stall-<UTC>/`:
`timeline.tsv`, `sse.txt`, `server.out|err`, `prompt.json`,
`xdg-data/opencode/log/opencode.log`.

## Environment notes (learned during the spike)

- Uses `opencode` **v2.0.15**; resolves the binary from `$PATH` at launch
  (`OPENCODE_BIN` overrides). Note `/opt/homebrew/bin/opencode` is v1.18.25 and
  does **not** speak the v2 API — the script resolves the binary before it
  restricts `PATH` for the child.
- `OPENCODE_MODELS_URL=http://127.0.0.1:1` + `OPENCODE_DISABLE_AUTOUPDATE=1`:
  a cold `opencode serve` otherwise fetches the model catalog from
  `models.opencode.ai`, which can hang; any session created during that window
  is permanently unpromptable. Pointing the source at a dead loopback forces the
  bundled snapshot and a deterministic ~1 s boot. Provider egress is unaffected
  (the provider uses its own base URL).
- Isolated `XDG_*`/`TMPDIR`/`HOME` under the run dir; the user's OpenCode state
  is never touched. Servers are torn down on exit.
