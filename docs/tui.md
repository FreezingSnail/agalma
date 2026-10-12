# `agalma watch` — full-screen TUI

`agalma watch` is the human-operated control plane for the Agalma factory: a
full-screen terminal view of every execution, component, cost row, and timeline
event, with the gated control actions (pause / resume / cancel) on keybindings.
It rides the existing read model (`agalma status --json`) and control plane; it
adds no privileges the CLI does not have.

Terminal stack: **ratatui 0.30.2 + crossterm 0.29.0** (exact `=` pins in the
workspace `[workspace.dependencies]`, used only by `agalma-conductor`). See
`docs/tui-design.md` §Terminal stack and `docs/m0-dependencies.md`.

## Usage

```sh
agalma watch                 # interactive full-screen view (requires a TTY)
agalma watch --once          # render a single frame, print it, exit 0
agalma watch --interval-ms 500
agalma watch --state-dir /path/to/state
```

- `--interval-ms <MS>` — refresh interval (default `1000`). Polling, not push:
  every tick the snapshot is rebuilt and the view re-rendered.
- `--once` — render exactly one frame through an off-screen backend and print
  it. Never enters the alternate screen or raw mode.
- `--state-dir <DIR>` — defaults to `AGALMA_STATE_DIR` or the platform
  application-support directory.

An absent ledger is not an error: the view reports `exists:false` with empty
panes and exits 0.

### Non-interactive / piped output

When stdout is not a terminal (piped or redirected), `watch` behaves as if
`--once` were passed: it renders a single frame and exits. This is the path used
by scripts and by the render-level tests.

## Panes

The body is a 2×2 grid; the footer lists the keybindings.

| Pane | Contents |
|---|---|
| **Executions** | one row per durable execution: id, phase, state, attempt, pending operations. The selected row is highlighted (`j`/`k`). |
| **Components** | per boundary: transport (`in-process`/`process`), `api@version`, socket liveness (`up`/`down`), bindings (`impl@ver State genN [caps] pins:N`), and watchdog/provider-health signals. |
| **Economics** | per execution: spend vs allowance, token totals, digest rollup, and parked/degradation alerts. Allowance is display-only (no enforcement). |
| **Timeline** | curated `execution_events` + digest milestones, oldest first, tail-windowed to the visible height. |

## Keybindings

| Key | Action |
|---|---|
| `q` / `Ctrl-C` | quit (restores the terminal) |
| `r` | refresh now |
| `Tab` / `Shift-Tab` | move focus to the next / previous pane |
| `1` `2` `3` `4` | focus Executions / Components / Economics / Timeline |
| `j` / `↓`, `k` / `↑` | move the execution selection |
| `p` | **pause** — set the durable kill latch (no typed confirmation) |
| `R` | **resume** — clear the latch and remove `STOP` (typed confirmation) |
| `c` / `x` | **cancel** the selected execution (typed confirmation) |
| `Esc` | cancel a pending confirmation |
| `Enter` | submit the typed confirmation |

The focused pane is marked with `▶` and a highlighted border.

## Control actions

Control is human-only and durable. Every attempt — applied or denied — writes
exactly one `control_receipt` (ledger schema v4); a cancel additionally records
the `cancel_requested` event. Nothing rewinds Git or the ledger: pause sets the
latch, resume clears it, cancel requests cancellation through
`ExecutionApi::cancel`. A denial (e.g. resume when nothing is stopped) is shown
in the footer and leaves state unchanged.

Destructive actions (`R` resume, `c`/`x` cancel) require the operator to type the
action name (`resume` / `cancel`) and press `Enter`. The typed input is kept on a
mismatch so it can be corrected; `Esc` abandons the action.

## Terminal lifecycle

`watch` enters raw mode and the alternate screen through a RAII guard. The guard
restores the cursor, alt screen, and raw mode on **every** exit path: normal
quit, an early error return, and unwinding panics (its `Drop` runs during
unwind). An additional panic hook restores the terminal before any other hook
runs, so a panic never leaves the shell in a broken state.
