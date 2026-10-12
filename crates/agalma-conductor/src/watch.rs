//! `agalma watch`: read-only live refresh loop over the TUI read model
//! (TUI.2, `agalma-3kr.3`).
//!
//! Renders the [`crate::status::Snapshot`] produced by the TUI.1 read model
//! (`agalma status --json`) into a refreshing terminal view: one row per
//! execution (phase / state / attempt / pending operations), plus the kill
//! latch, the harness binding, and the configured transports. The view is
//! **strictly read-only** — it never opens the ledger for writing and never
//! touches the state dir (see `docs/tui-design.md`, "Live transport").
//!
//! Transport is polling, not push: every interval we rebuild the snapshot
//! through [`crate::status::snapshot`] and re-render. Terminal handling is
//! **std-only ANSI** for v1: no new dependency, no `ratatui`/`crossterm` until
//! the terminal stack is approved in TUI.6. The loop enters the alternate
//! screen, hides the cursor, and restores both on every exit path (quit key,
//! SIGINT/SIGTERM, EOF). `q` is read from canonical stdin, so it is confirmed
//! with Enter in v1; Ctrl-C exits immediately.
//!
//! When stdout is not a terminal, or `--once` is passed, the view degrades to a
//! single frame and exits: this is the non-interactive (pipable) path used by
//! the focused verification.

use std::io::{IsTerminal, Write};
use std::path::Path;
use std::process::ExitCode;
use std::time::Duration;

use tokio::signal::unix::{signal, SignalKind};
use tokio::time::MissedTickBehavior;

use crate::cli::WatchArgs;
use crate::status::Snapshot;

/// Enter the alternate screen and hide the cursor.
const ENTER_VIEW: &str = "\x1b[?1049h\x1b[?25l";
/// Restore the cursor and leave the alternate screen.
const LEAVE_VIEW: &str = "\x1b[?25h\x1b[?1049l";
/// Clear the screen and home the cursor (one frame start).
const CLEAR_HOME: &str = "\x1b[2J\x1b[H";

const RESET: &str = "\x1b[0m";
const BOLD: &str = "\x1b[1m";
const DIM: &str = "\x1b[2m";
const RED: &str = "\x1b[31m";
const GREEN: &str = "\x1b[32m";
const YELLOW: &str = "\x1b[33m";
const CYAN: &str = "\x1b[36m";

/// `agalma watch`: resolve the state dir, then render the read model.
///
/// Interactive (TTY, no `--once`): alternate-screen refresh loop until `q`,
/// Ctrl-C/SIGTERM, or stdin EOF. Otherwise a single frame is printed and the
/// command exits 0 (absent ledger included).
pub async fn run(args: &WatchArgs) -> ExitCode {
    let state_dir = match crate::config::resolve_state_dir(args.state_dir.as_deref()) {
        Ok(dir) => dir,
        Err(err) => {
            eprintln!("agalma: {err}");
            return ExitCode::FAILURE;
        }
    };

    if args.once || !std::io::stdout().is_terminal() {
        return match once_frame(&state_dir, args.interval_ms) {
            Ok(frame) => {
                print!("{frame}");
                let _ = std::io::stdout().flush();
                ExitCode::SUCCESS
            }
            Err(err) => {
                eprintln!("agalma: {err}");
                ExitCode::FAILURE
            }
        };
    }

    run_loop(&state_dir, args.interval_ms).await
}

/// Build one frame for `state_dir` (read model + render). Pure apart from the
/// read-only snapshot; used by both the loop and the `--once` path.
pub fn once_frame(state_dir: &Path, interval_ms: u64) -> Result<String, String> {
    let transports = crate::components::Transports::from_env();
    let snapshot = crate::status::snapshot_with_transports(state_dir, &transports)?;
    let (width, height) = terminal_size();
    let ctx = FrameContext {
        width,
        height,
        interval_ms,
        ledger_transport: transports.ledger,
        harness_transport: transports.harness,
    };
    Ok(render(&snapshot, &ctx))
}

/// Render parameters that do not come from the read model.
#[derive(Debug, Clone, Copy)]
pub struct FrameContext<'a> {
    /// Visible columns.
    pub width: usize,
    /// Visible rows.
    pub height: usize,
    /// Refresh interval printed in the footer.
    pub interval_ms: u64,
    /// Ledger transport selector (informational).
    pub ledger_transport: &'a str,
    /// Harness transport selector (informational).
    pub harness_transport: &'a str,
}

/// Render the read model into an ANSI-styled frame.
///
/// The frame is a plain string (no trailing newline required) so it can be
/// asserted in tests and printed verbatim. It always carries an `exists:`
/// token and never panics on an absent or empty snapshot.
pub fn render(snapshot: &Snapshot, ctx: &FrameContext<'_>) -> String {
    let width = ctx.width.max(40);
    let mut out = String::new();

    // Title + state dir.
    out.push_str(&format!(
        "\n{BOLD}{CYAN}agalma watch{RESET}  {DIM}{}{RESET}\n",
        clip(&snapshot.state_dir, width.saturating_sub(16))
    ));

    // Presence + schema + binding.
    if snapshot.exists {
        out.push_str(&format!("  {GREEN}exists:true{RESET}\n"));
    } else {
        out.push_str(&format!(
            "  {YELLOW}exists:false{RESET} {DIM}— no ledger present{RESET}\n"
        ));
    }
    let schema = snapshot
        .schema_version
        .map(|v| v.to_string())
        .unwrap_or_else(|| "-".to_string());
    let binding = snapshot.binding.as_deref().unwrap_or("-");
    out.push_str(&format!(
        "  {DIM}schema:{RESET} {schema}   {DIM}binding:{RESET} {CYAN}{}{RESET}\n",
        clip(binding, width.saturating_sub(24))
    ));

    // Kill latch + transports.
    let latch = if snapshot.kill_latch {
        format!("{BOLD}{RED}true{RESET}")
    } else {
        format!("{GREEN}false{RESET}")
    };
    out.push_str(&format!(
        "  {DIM}kill_latch:{RESET} {latch}   {DIM}ledger:{RESET} {}   {DIM}harness:{RESET} {}\n",
        ctx.ledger_transport, ctx.harness_transport
    ));

    out.push_str(&format!("{DIM}{}{RESET}\n", rule(width)));

    // Executions table.
    let (id_w, phase_w, state_w, try_w, pending_w) = columns(width);
    out.push_str(&format!(
        "{BOLD}{:<id_w$} {:<phase_w$} {:<state_w$} {:>try_w$} {:<pending_w$}{RESET}\n",
        clip("EXECUTION", id_w),
        clip("PHASE", phase_w),
        clip("STATE", state_w),
        "TRY",
        clip("PENDING", pending_w),
    ));

    // Rows that fit above the footer (header uses 5 lines, footer 2).
    let max_rows = ctx.height.saturating_sub(7).max(1);
    if snapshot.executions.is_empty() {
        out.push_str(&format!("{DIM}{}{RESET}\n", "  (no executions)"));
    } else {
        for (index, row) in snapshot.executions.iter().enumerate() {
            if index >= max_rows {
                let more = snapshot.executions.len() - index;
                out.push_str(&format!("{DIM}  … {more} more{RESET}\n"));
                break;
            }
            let pending = pending_text(row.pending_operations.len(), &row.pending_operations);
            out.push_str(&format!(
                "{} {:<phase_w$} {} {:>try_w$} {}\n",
                styled_cell(&row.execution_id, id_w, CYAN),
                clip(&row.phase, phase_w),
                styled_state(&row.state, state_w),
                row.attempt.to_string(),
                clip(&pending, pending_w),
            ));
        }
    }

    out.push_str(&format!("{DIM}{}{RESET}\n", rule(width)));
    out.push_str(&render_components(snapshot));
    out.push_str(&format!("{DIM}{}{RESET}\n", rule(width)));
    out.push_str(&format!(
        "{DIM}{} execution(s) · {} component(s) · {} ms · q + Enter quits · Ctrl-C exits{RESET}",
        snapshot.executions.len(),
        snapshot.components.len(),
        ctx.interval_ms
    ));

    // Final guard: clip every visible line to the terminal width while keeping
    // ANSI sequences intact, so narrow terminals never wrap or overflow.
    out.split('\n')
        .map(|line| clip_line_ansi(line, width))
        .collect::<Vec<_>>()
        .join("\n")
}

/// Interactive refresh loop: render, then wait for a tick, quit key, or
/// resize/signal. Restores the terminal on every exit path.
async fn run_loop(state_dir: &Path, interval_ms: u64) -> ExitCode {
    let mut ticker = tokio::time::interval(Duration::from_millis(interval_ms.max(1)));
    ticker.set_missed_tick_behavior(MissedTickBehavior::Skip);
    // The first tick completes immediately; consume it so the loop body runs
    // once now and then once per interval.
    ticker.tick().await;

    let mut sigint = match signal(SignalKind::interrupt()) {
        Ok(stream) => stream,
        Err(err) => {
            eprintln!("agalma: cannot install SIGINT handler: {err}");
            return ExitCode::FAILURE;
        }
    };
    let mut sigterm = match signal(SignalKind::terminate()) {
        Ok(stream) => stream,
        Err(err) => {
            eprintln!("agalma: cannot install SIGTERM handler: {err}");
            return ExitCode::FAILURE;
        }
    };
    let mut sigwinch = match signal(SignalKind::window_change()) {
        Ok(stream) => stream,
        Err(err) => {
            eprintln!("agalma: cannot install SIGWINCH handler: {err}");
            return ExitCode::FAILURE;
        }
    };

    // `q` + Enter on canonical stdin: a background reader signals a quit.
    let (quit_tx, mut quit_rx) = tokio::sync::mpsc::channel::<()>(1);
    std::thread::spawn(move || {
        use std::io::BufRead;
        let stdin = std::io::stdin();
        for line in stdin.lock().lines() {
            match line {
                Ok(line) if is_quit(&line) => {
                    let _ = quit_tx.blocking_send(());
                    break;
                }
                Ok(_) => {}
                Err(_) => {
                    let _ = quit_tx.blocking_send(());
                    break;
                }
            }
        }
    });

    let mut stdout = std::io::stdout();
    let _ = write!(stdout, "{ENTER_VIEW}");
    let _ = stdout.flush();

    loop {
        let transports = crate::components::Transports::from_env();
        let frame = match crate::status::snapshot_with_transports(state_dir, &transports) {
            Ok(snapshot) => {
                let (width, height) = terminal_size();
                render(
                    &snapshot,
                    &FrameContext {
                        width,
                        height,
                        interval_ms,
                        ledger_transport: transports.ledger,
                        harness_transport: transports.harness,
                    },
                )
            }
            Err(err) => format!("{CLEAR_HOME}{RED}agalma watch: {err}{RESET}\n"),
        };
        let _ = write!(stdout, "{CLEAR_HOME}{frame}");
        let _ = stdout.flush();

        tokio::select! {
            _ = ticker.tick() => {}
            _ = sigint.recv() => break,
            _ = sigterm.recv() => break,
            _ = sigwinch.recv() => {}
            _ = quit_rx.recv() => break,
        }
    }

    let _ = write!(stdout, "{LEAVE_VIEW}");
    let _ = stdout.flush();
    ExitCode::SUCCESS
}

/// A stdin line that requests a quit (`q`/`Q`/`quit`, whitespace-trimmed).
fn is_quit(line: &str) -> bool {
    matches!(line.trim(), "q" | "Q" | "quit" | "QUIT" | "Quit")
}

/// Column budgets for the executions table.
fn columns(width: usize) -> (usize, usize, usize, usize, usize) {
    let id_w = (width * 2 / 5).clamp(20, 48);
    let phase_w = 10;
    let state_w = 12;
    let try_w = 4;
    let pending_w = width
        .saturating_sub(id_w + phase_w + state_w + try_w + 4)
        .max(6);
    (id_w, phase_w, state_w, try_w, pending_w)
}

/// `N` plus the first pending operation id, or `-` when there are none.
fn pending_text(count: usize, operations: &[String]) -> String {
    match operations.first() {
        Some(first) if count == 1 => first.clone(),
        Some(first) => format!("{count} ({first})"),
        None => "-".to_string(),
    }
}

/// The per-component panel: transport, API version, socket liveness, bindings,
/// and watchdog/provider-health signals (TUI.3).
fn render_components(snapshot: &Snapshot) -> String {
    if snapshot.components.is_empty() {
        return String::new();
    }
    let mut out = String::new();
    out.push_str(&format!(
        "{BOLD}COMPONENTS{RESET}  {DIM}transport · api@version · socket(up|down) · bindings · health{RESET}\n"
    ));
    for component in &snapshot.components {
        let version = component
            .api_version
            .map(|v| v.to_string())
            .unwrap_or_else(|| "-".to_string());
        let api = format!("{}@{}", component.api, version);
        let socket = match &component.socket {
            Some(name) if component.socket_present => format!("{name} up"),
            Some(name) => format!("{name} down"),
            None => "-".to_string(),
        };
        let socket_style = if component.socket_present { GREEN } else { DIM };
        out.push_str(&format!(
            "  {CYAN}{:<10}{RESET} {:<10} {:<16} {socket_style}{:<20}{RESET} {}\n",
            clip(&component.component, 10),
            component.transport,
            clip(&api, 16),
            clip(&socket, 20),
            component_binding_text(component),
        ));
        if component.stalls > 0 {
            let reason = component.last_health_reason.as_deref().unwrap_or("-");
            out.push_str(&format!(
                "    {YELLOW}health:{RESET} {} stall(s) · last: {reason}\n",
                component.stalls
            ));
        }
    }
    out
}

/// One-line binding summary for a component (`impl@ver State genN [caps] pins:N`).
fn component_binding_text(component: &crate::components::ComponentSnapshot) -> String {
    if component.bindings.is_empty() {
        return "-".to_string();
    }
    component
        .bindings
        .iter()
        .map(|binding| {
            let caps = if binding.capabilities.is_empty() {
                "-".to_string()
            } else {
                binding.capabilities.join(",")
            };
            let pins = if binding.pinned_operations.is_empty() {
                String::new()
            } else {
                format!(" pins:{}", binding.pinned_operations.len())
            };
            format!(
                "{}@{} {} gen{} [{}]{}",
                binding.implementation,
                binding.implementation_version,
                binding.state,
                binding.generation,
                caps,
                pins
            )
        })
        .collect::<Vec<_>>()
        .join(" · ")
}

/// A padded, clipped cell carrying a fixed style.
fn styled_cell(text: &str, width: usize, style: &str) -> String {
    format!("{style}{:<width$}{RESET}", clip(text, width))
}

/// A clipped `STATE` cell colored by state.
fn styled_state(state: &str, width: usize) -> String {
    let style = match state {
        "Running" | "Dispatched" | "Completed" | "Done" => GREEN,
        "Parked" => YELLOW,
        "Failed" => RED,
        _ => CYAN,
    };
    format!("{style}{:<width$}{RESET}", clip(state, width))
}

/// Clip a line to `width` visible characters, leaving ANSI sequences verbatim
/// (they do not count toward the width). Truncation appends a reset so no style
/// leaks past the cut.
fn clip_line_ansi(line: &str, width: usize) -> String {
    let mut out = String::new();
    let mut visible = 0usize;
    let mut chars = line.chars().peekable();
    while let Some(c) = chars.next() {
        if c == '\x1b' {
            out.push(c);
            while let Some(&next) = chars.peek() {
                out.push(next);
                chars.next();
                if next.is_ascii_alphabetic() {
                    break;
                }
            }
            continue;
        }
        if visible >= width {
            out.push_str(RESET);
            break;
        }
        out.push(c);
        visible += 1;
    }
    out
}

/// Truncate `text` to `width` display characters, adding an ellipsis when
/// clipped. Char-based (not byte-based) so multibyte text is not split.
fn clip(text: &str, width: usize) -> String {
    if width == 0 {
        return String::new();
    }
    let chars: Vec<char> = text.chars().collect();
    if chars.len() <= width {
        return text.to_string();
    }
    if width == 1 {
        return "…".to_string();
    }
    let mut clipped: String = chars[..width - 1].iter().collect();
    clipped.push('…');
    clipped
}

/// Horizontal rule sized to `width` (capped so narrow terminals stay sane).
fn rule(width: usize) -> String {
    "─".repeat(width.clamp(20, 120))
}

/// Visible terminal size `(columns, rows)`.
///
/// Prefers `stty size` (authoritative on SIGWINCH), then `COLUMNS`/`LINES`,
/// then a `80x24` default. Read-only: spawns `stty` at most, never writes.
fn terminal_size() -> (usize, usize) {
    if let Ok(out) = std::process::Command::new("stty").arg("size").output() {
        if out.status.success() {
            let text = String::from_utf8_lossy(&out.stdout);
            let mut parts = text.split_whitespace();
            if let (Some(rows), Some(cols)) = (parts.next(), parts.next()) {
                if let (Ok(rows), Ok(cols)) = (rows.parse::<usize>(), cols.parse::<usize>()) {
                    if rows > 0 && cols > 0 {
                        return (cols, rows);
                    }
                }
            }
        }
    }
    let env_size = (|| {
        let cols = std::env::var("COLUMNS")
            .ok()?
            .trim()
            .parse::<usize>()
            .ok()?;
        let rows = std::env::var("LINES").ok()?.trim().parse::<usize>().ok()?;
        Some((cols, rows))
    })();
    env_size
        .filter(|(c, r)| *c > 0 && *r > 0)
        .unwrap_or((80, 24))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::components::{BindingView, ComponentSnapshot};
    use crate::status::{ExecutionSnapshot, Snapshot};
    use agalma_testkit::ScratchDir;

    fn ctx(width: usize, height: usize, interval_ms: u64) -> FrameContext<'static> {
        FrameContext {
            width,
            height,
            interval_ms,
            ledger_transport: "in-process",
            harness_transport: "in-process",
        }
    }

    /// Empty snapshot for an absent ledger.
    fn absent() -> Snapshot {
        Snapshot {
            state_dir: "/var/tmp/agalma-watch-absent".to_string(),
            exists: false,
            kill_latch: false,
            schema_version: None,
            binding: None,
            executions: Vec::new(),
            components: Vec::new(),
        }
    }

    /// Two components: a bound harness (with health signals) and a bare ledger.
    fn components() -> Vec<ComponentSnapshot> {
        vec![
            ComponentSnapshot {
                component: "ledger".to_string(),
                api: "LedgerApi".to_string(),
                api_version: Some(4),
                transport: "in-process".to_string(),
                socket: Some("ledger.sock".to_string()),
                socket_present: false,
                bindings: Vec::new(),
                stalls: 0,
                last_health_reason: None,
            },
            ComponentSnapshot {
                component: "harness".to_string(),
                api: "HarnessApi".to_string(),
                api_version: Some(0),
                transport: "process".to_string(),
                socket: Some("harness*.sock".to_string()),
                socket_present: true,
                bindings: vec![BindingView {
                    binding_id: "binding:reference:1".to_string(),
                    implementation: "reference".to_string(),
                    implementation_version: "reference/v1".to_string(),
                    api_version: 0,
                    generation: 1,
                    state: "Active".to_string(),
                    capabilities: vec!["context_hooks".to_string()],
                    pinned_operations: vec!["op:build@2".to_string()],
                }],
                stalls: 2,
                last_health_reason: Some("session_gone".to_string()),
            },
        ]
    }

    /// One running execution with two pending operations and a binding.
    fn present() -> Snapshot {
        Snapshot {
            state_dir: "/var/tmp/agalma-watch-present".to_string(),
            exists: true,
            kill_latch: true,
            schema_version: Some(4),
            binding: Some("reference@reference/v1".to_string()),
            executions: vec![ExecutionSnapshot {
                execution_id: "exec:task-a:1".to_string(),
                phase: "Build".to_string(),
                state: "Running".to_string(),
                attempt: 2,
                pending_operations: vec![
                    "op:exec:task-a:1:build@2".to_string(),
                    "op:exec:task-a:1:build@3".to_string(),
                ],
                parked_reason: None,
                harness: "reference@reference/v1".to_string(),
            }],
            components: components(),
        }
    }

    /// Strip ANSI SGR/CSI sequences so tests can assert on visible text.
    fn strip_ansi(text: &str) -> String {
        let mut out = String::new();
        let mut chars = text.chars().peekable();
        while let Some(c) = chars.next() {
            if c == '\x1b' {
                if chars.peek() == Some(&'[') {
                    chars.next();
                    for c in chars.by_ref() {
                        if c.is_ascii_alphabetic() {
                            break;
                        }
                    }
                }
            } else {
                out.push(c);
            }
        }
        out
    }

    #[test]
    fn absent_snapshot_renders_exists_false() {
        let frame = render(&absent(), &ctx(100, 24, 1000));
        let plain = strip_ansi(&frame);
        assert!(plain.contains("exists:false"), "{plain}");
        assert!(plain.contains("no ledger present"), "{plain}");
        assert!(plain.contains("(no executions)"), "{plain}");
        assert!(plain.contains("binding: -"), "{plain}");
    }

    #[test]
    fn present_snapshot_renders_rows_and_status() {
        let frame = render(&present(), &ctx(120, 24, 1500));
        let plain = strip_ansi(&frame);
        assert!(plain.contains("exists:true"), "{plain}");
        assert!(plain.contains("kill_latch: true"), "{plain}");
        assert!(plain.contains("binding: reference@reference/v1"), "{plain}");
        assert!(plain.contains("ledger: in-process"), "{plain}");
        assert!(plain.contains("harness: in-process"), "{plain}");
        assert!(plain.contains("exec:task-a:1"), "{plain}");
        assert!(plain.contains("Build"), "{plain}");
        assert!(plain.contains("Running"), "{plain}");
        assert!(plain.contains("PENDING"), "{plain}");
        assert!(plain.contains("1500 ms"), "{plain}");
    }

    #[test]
    fn present_snapshot_renders_component_panel() {
        let frame = render(&present(), &ctx(140, 30, 1000));
        let plain = strip_ansi(&frame);
        // Demo frame for the TUI.3 report (run with --nocapture to view).
        println!("--- TUI.3 component panel demo ---\n{plain}");
        assert!(plain.contains("COMPONENTS"), "{plain}");
        assert!(plain.contains("ledger"), "{plain}");
        assert!(plain.contains("harness"), "{plain}");
        // Socket liveness proxy: harness*.sock up, ledger.sock down.
        assert!(plain.contains("harness*.sock up"), "{plain}");
        assert!(plain.contains("ledger.sock down"), "{plain}");
        // Binding state + generation + capability + pinned operation count.
        assert!(
            plain.contains("reference@reference/v1 Active gen1"),
            "{plain}"
        );
        assert!(plain.contains("[context_hooks]"), "{plain}");
        assert!(plain.contains("pins:1"), "{plain}");
        // Provider/watchdog health signal.
        assert!(plain.contains("health:"), "{plain}");
        assert!(plain.contains("2 stall(s)"), "{plain}");
        assert!(plain.contains("last: session_gone"), "{plain}");
        // Transport mode is visible per component.
        assert!(plain.contains("process"), "{plain}");
    }

    #[test]
    fn absent_snapshot_has_no_component_panel() {
        let plain = strip_ansi(&render(&absent(), &ctx(120, 24, 1000)));
        assert!(!plain.contains("COMPONENTS"), "{plain}");
        assert!(plain.contains("0 component(s)"), "{plain}");
    }

    #[test]
    fn kill_latch_and_state_are_styled() {
        let plain_absent = strip_ansi(&render(&absent(), &ctx(120, 24, 1000)));
        assert!(plain_absent.contains("kill_latch: false"), "{plain_absent}");
        let frame = render(&present(), &ctx(120, 24, 1000));
        // Red+bold latch when set; green elsewhere.
        assert!(frame.contains("\x1b[1m\x1b[31mtrue\x1b[0m"), "{frame:?}");
        assert!(frame.contains("\x1b[32mRunning"), "{frame:?}");
    }

    #[test]
    fn narrow_frames_do_not_overflow_width() {
        for width in [40usize, 60, 200] {
            let frame = render(&present(), &ctx(width, 24, 1000));
            for line in strip_ansi(&frame).lines() {
                // The rule uses its own (clamped) width; allow a small margin.
                assert!(
                    line.chars().count() <= width.max(20) + 2,
                    "width={width} line too long ({}): {line:?}",
                    line.chars().count()
                );
            }
        }
    }

    #[test]
    fn row_budget_respects_height() {
        let mut snapshot = present();
        snapshot.executions = (0..20)
            .map(|i| ExecutionSnapshot {
                execution_id: format!("exec:task-{i}:1"),
                phase: "Build".to_string(),
                state: "Running".to_string(),
                attempt: 1,
                pending_operations: vec![],
                parked_reason: None,
                harness: "reference@reference/v1".to_string(),
            })
            .collect();
        let plain = strip_ansi(&render(&snapshot, &ctx(120, 10, 1000)));
        assert!(
            plain.contains("more"),
            "height budget should fold rows: {plain}"
        );
    }

    #[test]
    fn is_quit_matches_only_quit_tokens() {
        for yes in ["q", "Q", "quit", "  Q  ", "Quit"] {
            assert!(is_quit(yes), "{yes:?} should quit");
        }
        for no in ["", "x", "quitx", "resume", "1"] {
            assert!(!is_quit(no), "{no:?} should not quit");
        }
    }

    #[test]
    fn once_frame_reads_absent_dir_without_writing() {
        let dir = ScratchDir::new("watch-absent-once");
        let frame = once_frame(dir.path(), 1000).expect("once frame");
        let plain = strip_ansi(&frame);
        assert!(plain.contains("exists:false"), "{plain}");

        // Acceptance: strictly no writes — the state dir stays empty.
        let entries: Vec<_> = std::fs::read_dir(dir.path())
            .expect("read dir")
            .map(|e| e.expect("entry").file_name())
            .collect();
        assert!(entries.is_empty(), "watch wrote to state dir: {entries:?}");
    }
}
