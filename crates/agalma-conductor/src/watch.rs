//! `agalma watch`: full-screen TUI over the TUI read model (TUI.6, `agalma-3kr.7`).
//!
//! Renders the [`crate::status::Snapshot`] (the TUI.1 read model, `agalma status
//! --json`) into a full-screen [ratatui] composition with four panes —
//! **executions**, **components**, **economics**, and **timeline** — plus a
//! focus model, keybindings, and the human control actions
//! (pause/resume/cancel) wired from [`crate::control`]. Terminal stack is the
//! approved `ratatui 0.30.2` + `crossterm 0.29.0` pair (`docs/tui-design.md`,
//! "Terminal stack"; `docs/m0-dependencies.md`).
//!
//! Transport is polling, not push: every interval the snapshot is rebuilt through
//! [`crate::status::snapshot_with_transports`] and re-rendered. The view is
//! read-only except for the human control actions, which go through
//! [`crate::control::ControlPlane`] (durable receipts; no silent rewinds).
//!
//! ## Terminal lifecycle
//!
//! [`ModeGuard`] enters raw mode + the alternate screen on construction and
//! restores both on **every** exit path: normal quit, an early error return, and
//! unwinding panics (the guard's `Drop` runs during unwind). An additional panic
//! hook restores the terminal before any other hook runs. This replaces the
//! std-only ANSI handling of v1.
//!
//! ## Non-interactive path
//!
//! When stdout is not a terminal, or `--once` is passed, the view degrades to a
//! single frame rendered through [`ratatui::backend::TestBackend`] (no TTY) and
//! printed verbatim. Render-level tests assert against `TestBackend` frames.

use std::io::{self, IsTerminal, Write};
use std::path::{Path, PathBuf};
use std::process::ExitCode;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::time::{Duration, Instant};

use crossterm::event::{self, Event, KeyCode, KeyEvent, KeyEventKind, KeyModifiers};
use crossterm::execute;
use crossterm::terminal::{
    disable_raw_mode, enable_raw_mode, EnterAlternateScreen, LeaveAlternateScreen,
};
use ratatui::backend::{CrosstermBackend, TestBackend};
use ratatui::buffer::Buffer;
use ratatui::layout::{Constraint, Layout, Rect};
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span, Text};
use ratatui::widgets::{Block, Cell, Paragraph, Row, Table, TableState, Wrap};
use ratatui::{Frame, Terminal};

use agalma_contracts::{ContractError, ExecutionId};
use agalma_execution::{Executor, ExecutorConfig, EXECUTION_DEFINITION_VERSION};
use agalma_ledger::SqliteLedger;

use crate::cli::WatchArgs;
use crate::components::{ComponentSnapshot, ComponentView, PluginComponent, Transports, ViewBlock};
use crate::control::{
    new_action_id, Actor, ControlAction, ControlOutcome, ControlPlane, ControlRequest,
};
use crate::status::Snapshot;

/// The four composition panes, in focus order.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Pane {
    Executions,
    Components,
    Economics,
    Timeline,
}

impl Pane {
    /// Every pane in focus order.
    pub const ALL: [Pane; 4] = [
        Pane::Executions,
        Pane::Components,
        Pane::Economics,
        Pane::Timeline,
    ];

    /// Pane label used in the pane title.
    pub fn title(self) -> &'static str {
        match self {
            Pane::Executions => "Executions",
            Pane::Components => "Components",
            Pane::Economics => "Economics",
            Pane::Timeline => "Timeline",
        }
    }

    /// Zero-based focus index.
    pub fn index(self) -> usize {
        match self {
            Pane::Executions => 0,
            Pane::Components => 1,
            Pane::Economics => 2,
            Pane::Timeline => 3,
        }
    }

    /// Focus by index (out-of-range wraps).
    pub fn from_index(index: usize) -> Pane {
        Pane::ALL[index % Pane::ALL.len()]
    }

    /// Next pane, wrapping.
    pub fn next(self) -> Pane {
        Pane::from_index(self.index() + 1)
    }

    /// Previous pane, wrapping.
    pub fn prev(self) -> Pane {
        Pane::from_index(self.index() + Pane::ALL.len() - 1)
    }
}

/// The interaction mode: normal keybindings, or a typed confirmation prompt for
/// a destructive control action.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Mode {
    Normal,
    /// Collecting the typed confirmation phrase for `action`
    /// (`resume` / `cancel`).
    Confirm {
        action: ControlAction,
        input: String,
    },
}

/// Applies one control request (production: a durable ledger write; tests: a
/// recording double).
pub trait ControlDriver {
    fn apply(&mut self, request: &ControlRequest) -> Result<ControlOutcome, ContractError>;
}

/// Production [`ControlDriver`]: opens the state-dir ledger per action and runs
/// it through [`ControlPlane`] (guards + policy + durable receipt), mirroring
/// the CLI `pause`/`resume`/`cancel` paths. Pause/resume never touch the cancel
/// seam; cancel reuses the real [`Executor`], recording a durable per-execution
/// `cancel_requested` intent and leaving the global pause latch unchanged.
pub struct LedgerControlDriver {
    state_dir: PathBuf,
}

impl LedgerControlDriver {
    /// Bind the driver to a state dir.
    pub fn new(state_dir: impl Into<PathBuf>) -> Self {
        LedgerControlDriver {
            state_dir: state_dir.into(),
        }
    }
}

impl ControlDriver for LedgerControlDriver {
    fn apply(&mut self, request: &ControlRequest) -> Result<ControlOutcome, ContractError> {
        let ledger_path = self.state_dir.join("ledger.sqlite");
        // The plane records receipts through its own connection; the executor
        // (the cancel seam) holds a second, so both borrow `&mut` cleanly.
        let mut plane_ledger = SqliteLedger::open(&ledger_path)?;
        let exec_ledger = SqliteLedger::open(&ledger_path)?;
        let mut executor = Executor::new(
            exec_ledger,
            ExecutorConfig {
                max_attempts: 1,
                execution_definition_version: EXECUTION_DEFINITION_VERSION,
            },
        );
        let mut plane = ControlPlane::new(&mut plane_ledger, &mut executor, &self.state_dir);
        plane.apply(request)
    }
}

/// The full-screen TUI state: the read-model snapshot plus focus, selection,
/// interaction mode, and the last control/notice line.
#[derive(Debug, Clone)]
pub struct App {
    /// Latest read model (executions / components / economics / timeline).
    pub snapshot: Snapshot,
    /// Boundary transports (shown in the header; also pinned for refresh).
    pub transports: Transports,
    /// Refresh interval in milliseconds (footer).
    pub interval_ms: u64,
    /// Focused pane.
    pub focus: Pane,
    /// Selected execution row (index into `snapshot.executions`).
    pub selection: usize,
    /// Bounded scroll offset (rows) into the components pane.
    pub components_offset: usize,
    /// Interaction mode (normal / typed confirmation).
    pub mode: Mode,
    /// Last control outcome or notice, shown in the footer.
    pub status: Option<String>,
    /// Set by the quit keybinding; the loop exits after the next draw.
    pub quit: bool,
    /// Set by the refresh keybinding or by a control action; the loop rebuilds
    /// the snapshot before the next draw.
    pub refresh_requested: bool,
}

impl App {
    /// Build the app from a snapshot.
    pub fn new(snapshot: Snapshot, transports: Transports, interval_ms: u64) -> Self {
        let mut app = App {
            snapshot,
            transports,
            interval_ms,
            focus: Pane::Executions,
            selection: 0,
            components_offset: 0,
            mode: Mode::Normal,
            status: None,
            quit: false,
            refresh_requested: false,
        };
        app.clamp_selection();
        app
    }

    /// Rebuild the snapshot from `state_dir`, preserving focus and clamping the
    /// selection to the new row count.
    pub fn refresh(&mut self, state_dir: &Path) -> Result<(), String> {
        self.snapshot = crate::status::snapshot_with_transports(state_dir, &self.transports)?;
        self.clamp_selection();
        Ok(())
    }

    /// Keep the selection inside the current execution list.
    fn clamp_selection(&mut self) {
        let len = self.snapshot.executions.len();
        if len == 0 {
            self.selection = 0;
        } else if self.selection >= len {
            self.selection = len - 1;
        }
    }

    /// The execution the cursor currently selects, if any.
    pub fn selected_execution(&self) -> Option<ExecutionId> {
        self.snapshot
            .executions
            .get(self.selection)
            .map(|row| ExecutionId::new(row.execution_id.clone()))
    }

    /// Move the execution selection by `delta`, saturating at the ends.
    fn move_selection(&mut self, delta: isize) {
        let len = self.snapshot.executions.len();
        if len == 0 {
            self.selection = 0;
            return;
        }
        let next = self.selection as isize + delta;
        self.selection = next.clamp(0, len as isize - 1) as usize;
    }

    /// Scroll the components pane by `delta` rows, saturating at the ends.
    pub fn scroll_components(&mut self, delta: isize) {
        let total = component_lines(self).len();
        if total == 0 {
            self.components_offset = 0;
            return;
        }
        let next = self.components_offset as isize + delta;
        self.components_offset = next.clamp(0, total as isize - 1) as usize;
    }

    /// Handle one key event. Destructive actions collect a typed confirmation
    /// before they are applied through `driver`.
    pub fn handle_key(&mut self, key: KeyEvent, driver: &mut dyn ControlDriver) {
        // Ctrl-C always quits (raw mode delivers it as a key, not a signal).
        if key.modifiers.contains(KeyModifiers::CONTROL)
            && matches!(key.code, KeyCode::Char('c') | KeyCode::Char('C'))
        {
            self.quit = true;
            return;
        }

        if let Mode::Confirm { action, input } = self.mode.clone() {
            match key.code {
                KeyCode::Esc => {
                    self.mode = Mode::Normal;
                    self.status = Some("confirmation cancelled".to_string());
                }
                KeyCode::Backspace => {
                    let mut input = input;
                    input.pop();
                    self.mode = Mode::Confirm { action, input };
                }
                KeyCode::Enter => {
                    self.submit_confirmation(action, input, driver);
                }
                KeyCode::Char(c) if !key.modifiers.contains(KeyModifiers::CONTROL) => {
                    let mut input = input;
                    input.push(c);
                    self.mode = Mode::Confirm { action, input };
                }
                _ => {}
            }
            return;
        }

        match key.code {
            KeyCode::Char('q') | KeyCode::Char('Q') => self.quit = true,
            KeyCode::Tab => self.focus = self.focus.next(),
            KeyCode::BackTab => self.focus = self.focus.prev(),
            KeyCode::Char('1') => self.focus = Pane::Executions,
            KeyCode::Char('2') => self.focus = Pane::Components,
            KeyCode::Char('3') => self.focus = Pane::Economics,
            KeyCode::Char('4') => self.focus = Pane::Timeline,
            KeyCode::Char('j') | KeyCode::Down => self.move_selection(1),
            KeyCode::Char('k') | KeyCode::Up => self.move_selection(-1),
            KeyCode::Char('[') => self.scroll_components(-1),
            KeyCode::Char(']') => self.scroll_components(1),
            KeyCode::PageUp => self.scroll_components(-10),
            KeyCode::PageDown => self.scroll_components(10),
            KeyCode::Char('r') => {
                self.refresh_requested = true;
                self.status = Some("refresh requested".to_string());
            }
            KeyCode::Char('p') => self.apply_now(ControlAction::Pause, None, driver),
            KeyCode::Char('R') => self.begin_confirm(ControlAction::Resume),
            KeyCode::Char('c') | KeyCode::Char('x') => match self.selected_execution() {
                Some(execution) => self.begin_confirm(ControlAction::Cancel { execution }),
                None => self.status = Some("no execution selected to cancel".to_string()),
            },
            _ => {}
        }
    }

    /// Enter the typed-confirmation mode for a destructive action.
    fn begin_confirm(&mut self, action: ControlAction) {
        self.status = Some(format!("confirm: type \"{}\" then Enter", action.name()));
        self.mode = Mode::Confirm {
            action,
            input: String::new(),
        };
    }

    /// Validate the typed confirmation and, when it matches, apply the action.
    /// Pause carries no confirmation and is always applied directly.
    fn submit_confirmation(
        &mut self,
        action: ControlAction,
        input: String,
        driver: &mut dyn ControlDriver,
    ) {
        if crate::control::confirmed(&action, Some(&input)) {
            self.mode = Mode::Normal;
            self.apply_now(action, Some(input), driver);
        } else {
            let name = action.name();
            self.status = Some(format!("confirmation mismatch: type \"{name}\""));
            // Keep the typed input so the operator can correct it.
            self.mode = Mode::Confirm { action, input };
        }
    }

    /// Apply a control action as the human actor through `driver`, then request
    /// a refresh so the effect is visible.
    fn apply_now(
        &mut self,
        action: ControlAction,
        confirmation: Option<String>,
        driver: &mut dyn ControlDriver,
    ) {
        let mut request = ControlRequest::new(new_action_id(&action), action, Actor::Human);
        if let Some(typed) = confirmation {
            request = request.with_confirmation(typed);
        }
        match driver.apply(&request) {
            Ok(outcome) => {
                self.status = Some(control_status(&request, &outcome));
                self.mode = Mode::Normal;
                self.refresh_requested = true;
            }
            Err(err) => {
                self.status = Some(format!("{}: error: {err}", request.action.name()));
            }
        }
    }
}

/// One-line summary of a control outcome for the footer.
fn control_status(request: &ControlRequest, outcome: &ControlOutcome) -> String {
    match outcome {
        ControlOutcome::Applied => format!("{}: applied", request.action.name()),
        ControlOutcome::Denied { reason } => {
            format!("{}: denied: {reason}", request.action.name())
        }
    }
}

/// `agalma watch`: resolve the state dir, then render the read model.
///
/// Interactive (TTY, no `--once`): full-screen ratatui loop until `q`/Ctrl-C,
/// SIGTERM, or a fatal I/O error. Otherwise a single `TestBackend` frame is
/// printed and the command exits 0 (absent ledger included).
pub async fn run(args: &WatchArgs) -> ExitCode {
    let state_dir = match crate::config::resolve_state_dir(args.state_dir.as_deref()) {
        Ok(dir) => dir,
        Err(err) => {
            eprintln!("agalma: {err}");
            return ExitCode::FAILURE;
        }
    };

    if args.once || !io::stdout().is_terminal() {
        return match once_frame(&state_dir, args.interval_ms) {
            Ok(frame) => {
                println!("{frame}");
                let _ = io::stdout().flush();
                ExitCode::SUCCESS
            }
            Err(err) => {
                eprintln!("agalma: {err}");
                ExitCode::FAILURE
            }
        };
    }

    // A background signal watcher flips the stop flag; the blocking event loop
    // checks it each tick and always restores the terminal through the guard.
    let stop = Arc::new(AtomicBool::new(false));
    spawn_signal_watch(stop.clone());
    let interval = args.interval_ms;
    match tokio::task::spawn_blocking(move || run_interactive(&state_dir, interval, stop)).await {
        Ok(code) => code,
        Err(err) => {
            eprintln!("agalma watch: view task failed: {err}");
            ExitCode::FAILURE
        }
    }
}

/// Flip `stop` on SIGINT/SIGTERM so the blocking view can exit cleanly.
fn spawn_signal_watch(stop: Arc<AtomicBool>) {
    use tokio::signal::unix::{signal, SignalKind};
    tokio::spawn(async move {
        let mut sigint = match signal(SignalKind::interrupt()) {
            Ok(stream) => stream,
            Err(_) => return,
        };
        let mut sigterm = match signal(SignalKind::terminate()) {
            Ok(stream) => stream,
            Err(_) => return,
        };
        tokio::select! {
            _ = sigint.recv() => {}
            _ = sigterm.recv() => {}
        }
        stop.store(true, Ordering::SeqCst);
    });
}

/// Build one frame for `state_dir` (read model + render). Pure apart from the
/// read-only snapshot; used by both the loop and the `--once` path.
pub fn once_frame(state_dir: &Path, interval_ms: u64) -> Result<String, String> {
    let app = build_app(state_dir, interval_ms)?;
    let (width, height) = terminal_size();
    Ok(render_to_string(&app, width, height))
}

/// Build the app (snapshot + view parameters) for `state_dir`.
fn build_app(state_dir: &Path, interval_ms: u64) -> Result<App, String> {
    let transports = Transports::from_env();
    let snapshot = crate::status::snapshot_with_transports(state_dir, &transports)?;
    Ok(App::new(snapshot, transports, interval_ms))
}

/// Render `app` into an off-screen [`TestBackend`] of `width` x `height` and
/// return the visible text (trailing spaces trimmed). Render-level test seam and
/// the engine behind the `--once` frame.
pub fn render_to_string(app: &App, width: u16, height: u16) -> String {
    let backend = TestBackend::new(width.max(1), height.max(1));
    let mut terminal = Terminal::new(backend).expect("test backend is infallible");
    terminal
        .draw(|frame| draw(frame, app))
        .expect("test backend draw is infallible");
    buffer_text(terminal.backend().buffer())
}

/// The interactive view loop: raw mode + alternate screen, crossterm events,
/// periodic refresh, and the control actions. Blocking; restores on all exits.
fn run_interactive(state_dir: &Path, interval_ms: u64, stop: Arc<AtomicBool>) -> ExitCode {
    // The guard owns restoration: on normal return, early error return, or panic
    // unwinding. Installed first so even a later failure leaves a clean terminal.
    let _guard = match ModeGuard::enter(CrosstermMode) {
        Ok(guard) => guard,
        Err(err) => {
            eprintln!("agalma watch: cannot initialize terminal: {err}");
            return ExitCode::FAILURE;
        }
    };
    install_restore_panic_hook();

    let backend = CrosstermBackend::new(io::stdout());
    let mut terminal = match Terminal::new(backend) {
        Ok(terminal) => terminal,
        Err(err) => {
            eprintln!("agalma watch: cannot open terminal backend: {err}");
            return ExitCode::FAILURE;
        }
    };
    let _ = terminal.clear();

    let mut app = match build_app(state_dir, interval_ms) {
        Ok(app) => app,
        Err(err) => {
            eprintln!("agalma watch: {err}");
            return ExitCode::FAILURE;
        }
    };
    let mut driver = LedgerControlDriver::new(state_dir);

    let interval = Duration::from_millis(interval_ms.max(1));
    let mut last_refresh = Instant::now();

    loop {
        if app.refresh_requested || last_refresh.elapsed() >= interval {
            if let Err(err) = app.refresh(state_dir) {
                app.status = Some(format!("refresh failed: {err}"));
            }
            app.refresh_requested = false;
            last_refresh = Instant::now();
        }

        if let Err(err) = terminal.draw(|frame| draw(frame, &app)) {
            eprintln!("agalma watch: draw failed: {err}");
            return ExitCode::FAILURE;
        }

        if app.quit || stop.load(Ordering::SeqCst) {
            break;
        }

        // Wait for a key/resize event, or the next refresh tick, whichever first.
        let remaining = interval.saturating_sub(last_refresh.elapsed());
        match event::poll(remaining) {
            Ok(true) => match event::read() {
                Ok(Event::Key(key))
                    if matches!(key.kind, KeyEventKind::Press | KeyEventKind::Repeat) =>
                {
                    app.handle_key(key, &mut driver);
                }
                // Resize: the next draw re-reads the backend size.
                Ok(_) => {}
                Err(err) => {
                    eprintln!("agalma watch: input error: {err}");
                    break;
                }
            },
            Ok(false) => {}
            Err(err) => {
                eprintln!("agalma watch: poll error: {err}");
                break;
            }
        }
    }

    ExitCode::SUCCESS
}

/// Restore the terminal before any other panic hook runs.
fn install_restore_panic_hook() {
    let previous = std::panic::take_hook();
    std::panic::set_hook(Box::new(move |info| {
        let _ = restore_terminal();
        previous(info);
    }));
}

/// Actions that put the terminal into, and out of, the full-screen view.
///
/// Abstracted so the exit path can be exercised in tests without a TTY.
pub trait TerminalMode {
    /// Enter raw mode and the alternate screen.
    fn enter(&mut self) -> io::Result<()>;
    /// Leave the alternate screen and restore the original terminal mode.
    fn exit(&mut self) -> io::Result<()>;
}

/// The real crossterm-backed terminal mode.
pub struct CrosstermMode;

impl TerminalMode for CrosstermMode {
    fn enter(&mut self) -> io::Result<()> {
        enable_raw_mode()?;
        let mut stdout = io::stdout();
        execute!(stdout, EnterAlternateScreen, crossterm::cursor::Hide)
    }

    fn exit(&mut self) -> io::Result<()> {
        restore_terminal()
    }
}

/// Best-effort restore of raw mode + alternate screen (used by the guard, the
/// panic hook, and [`CrosstermMode::exit`]).
fn restore_terminal() -> io::Result<()> {
    let mut stdout = io::stdout();
    let _ = execute!(stdout, crossterm::cursor::Show, LeaveAlternateScreen);
    disable_raw_mode()
}

/// RAII guard: enters the terminal mode on construction and restores it on
/// `Drop` (normal return, early return, or panic unwind).
pub struct ModeGuard<M: TerminalMode> {
    mode: Option<M>,
}

impl<M: TerminalMode> ModeGuard<M> {
    /// Enter `mode`; on failure the terminal is left untouched.
    pub fn enter(mut mode: M) -> io::Result<Self> {
        mode.enter()?;
        Ok(ModeGuard { mode: Some(mode) })
    }
}

impl<M: TerminalMode> Drop for ModeGuard<M> {
    fn drop(&mut self) {
        if let Some(mut mode) = self.mode.take() {
            let _ = mode.exit();
        }
    }
}

/// Draw the whole composition.
pub fn draw(frame: &mut Frame<'_>, app: &App) {
    let [header, body, footer] = Layout::vertical([
        Constraint::Length(3),
        Constraint::Min(0),
        Constraint::Length(2),
    ])
    .areas(frame.area());

    draw_header(frame, app, header);
    draw_body(frame, app, body);
    draw_footer(frame, app, footer);
}

/// Header: title, state dir, presence/schema/binding/latch, transports.
fn draw_header(frame: &mut Frame<'_>, app: &App, area: Rect) {
    let snapshot = &app.snapshot;
    let schema = snapshot
        .schema_version
        .map(|v| v.to_string())
        .unwrap_or_else(|| "-".to_string());
    let binding = snapshot.binding.as_deref().unwrap_or("-");
    let (exists_text, exists_style) = if snapshot.exists {
        ("exists:true", Style::new().fg(Color::Green))
    } else {
        ("exists:false", Style::new().fg(Color::Yellow))
    };
    let latch = if snapshot.kill_latch {
        Span::styled(
            "kill_latch: true",
            Style::new().fg(Color::Red).add_modifier(Modifier::BOLD),
        )
    } else {
        Span::styled("kill_latch: false", Style::new().fg(Color::Green))
    };

    let lines = vec![
        Line::from(vec![
            Span::styled(
                "agalma watch",
                Style::new().fg(Color::Cyan).add_modifier(Modifier::BOLD),
            ),
            Span::styled(
                format!("  ·  {}", snapshot.state_dir),
                Style::new().fg(Color::DarkGray),
            ),
        ]),
        Line::from(vec![
            Span::styled(exists_text, exists_style),
            Span::styled(
                format!("   schema: {schema}   binding: {binding}   "),
                Style::new().fg(Color::Gray),
            ),
            latch,
        ]),
        Line::from(Span::styled(
            format!(
                "ledger: {}   harness: {}   interval: {} ms   executions: {}",
                app.transports.ledger,
                app.transports.harness,
                app.interval_ms,
                snapshot.executions.len()
            ),
            Style::new().fg(Color::DarkGray),
        )),
    ];
    frame.render_widget(Paragraph::new(Text::from(lines)), area);
}

/// Body: 2x2 grid of the four panes.
fn draw_body(frame: &mut Frame<'_>, app: &App, area: Rect) {
    let [left, right] =
        Layout::horizontal([Constraint::Percentage(55), Constraint::Percentage(45)]).areas(area);
    let [executions, economics] =
        Layout::vertical([Constraint::Percentage(58), Constraint::Percentage(42)]).areas(left);
    let [components, timeline] =
        Layout::vertical([Constraint::Percentage(52), Constraint::Percentage(48)]).areas(right);

    draw_executions(frame, app, executions);
    draw_components(frame, app, components);
    draw_economics(frame, app, economics);
    draw_timeline(frame, app, timeline);
}

/// A bordered pane block, marked and highlighted when focused.
fn pane_block(title: &str, focused: bool) -> Block<'static> {
    let border = if focused {
        Style::new().fg(Color::Yellow)
    } else {
        Style::new().fg(Color::DarkGray)
    };
    let title_style = if focused {
        Style::new().fg(Color::Yellow).add_modifier(Modifier::BOLD)
    } else {
        Style::new().fg(Color::Gray)
    };
    let marker = if focused { "▶ " } else { "  " };
    Block::bordered()
        .title(Line::from(Span::styled(
            format!("{marker}{title}"),
            title_style,
        )))
        .border_style(border)
}

/// Executions pane: a table with the selected row highlighted.
fn draw_executions(frame: &mut Frame<'_>, app: &App, area: Rect) {
    let rows: Vec<Row> = app
        .snapshot
        .executions
        .iter()
        .map(|row| {
            Row::new(vec![
                Cell::from(row.execution_id.clone()),
                Cell::from(row.phase.clone()),
                Cell::from(row.state.clone()).style(state_style(&row.state)),
                Cell::from(row.attempt.to_string()),
                Cell::from(pending_text(
                    row.pending_operations.len(),
                    &row.pending_operations,
                )),
            ])
        })
        .collect();

    let header = Row::new(vec!["EXECUTION", "PHASE", "STATE", "TRY", "PENDING"])
        .style(Style::new().add_modifier(Modifier::BOLD));
    let widths = [
        Constraint::Percentage(34),
        Constraint::Length(8),
        Constraint::Length(11),
        Constraint::Length(4),
        Constraint::Min(6),
    ];
    let table = Table::new(rows, widths)
        .header(header)
        .block(pane_block(
            Pane::Executions.title(),
            app.focus == Pane::Executions,
        ))
        .row_highlight_style(Style::new().add_modifier(Modifier::REVERSED));
    let mut state = TableState::default().with_selected(if app.snapshot.executions.is_empty() {
        None
    } else {
        Some(app.selection)
    });
    frame.render_stateful_widget(table, area, &mut state);
}

/// The visible `[start, end)` slice of the components pane plus its marker
/// flags, for a given scroll offset and pane height. Distinct from
/// [`draw_components`] so the boundary is directly testable.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
struct ComponentsWindow {
    start: usize,
    end: usize,
    earlier: bool,
    more: bool,
}

/// Compute the bounded-navigation window.
///
/// When every row fits (`total <= max_rows`) the whole list is shown with no
/// markers, so no line vanishes. Otherwise the window reserves up to one row
/// each for the "earlier" and "more" markers, and the offset is clamped so the
/// final window ends exactly at `total`: every item stays reachable.
fn components_window(total: usize, offset: usize, max_rows: usize) -> ComponentsWindow {
    if total == 0 {
        return ComponentsWindow {
            start: 0,
            end: 0,
            earlier: false,
            more: false,
        };
    }
    if total <= max_rows {
        return ComponentsWindow {
            start: 0,
            end: total,
            earlier: false,
            more: false,
        };
    }
    let window = max_rows.saturating_sub(2).max(1);
    let start = offset.min(total - window);
    let end = start + window;
    ComponentsWindow {
        start,
        end,
        earlier: start > 0,
        more: end < total,
    }
}

/// Components pane (TUI.3 + HP.6): boundary components, then registry/report
/// driven plugin components and their generic declarative views, windowed to the
/// visible height with bounded scroll navigation.
fn draw_components(frame: &mut Frame<'_>, app: &App, area: Rect) {
    let lines = component_lines(app);
    let inner_height = area.height.saturating_sub(2) as usize;
    let max_rows = inner_height.max(1);
    let total = lines.len();

    let mut body: Vec<Line> = Vec::new();
    if total == 0 {
        body.push(Line::from(Span::styled(
            "(no components)",
            Style::new().fg(Color::DarkGray),
        )));
    } else {
        let window = components_window(total, app.components_offset, max_rows);
        if window.earlier {
            body.push(scroll_marker(format!("… {} earlier", window.start)));
        }
        body.extend(lines[window.start..window.end].iter().cloned());
        if window.more {
            body.push(scroll_marker(format!("… {} more", total - window.end)));
        }
    }

    // No wrapping: one logical line per pane row keeps bounded navigation
    // coherent (long view text is truncated, which is itself a bound).
    let pane = Paragraph::new(Text::from(body)).block(pane_block(
        Pane::Components.title(),
        app.focus == Pane::Components,
    ));
    frame.render_widget(pane, area);
}

/// A dim "N earlier / N more" bounded-navigation marker.
fn scroll_marker(text: String) -> Line<'static> {
    Line::from(Span::styled(text, Style::new().fg(Color::DarkGray)))
}

/// The number of rows the components pane currently holds. Bounded-navigation
/// callers use this to clamp the scroll offset.
pub fn component_line_count(app: &App) -> usize {
    component_lines(app).len()
}

/// Build every components-pane line: boundary components then plugin components.
fn component_lines(app: &App) -> Vec<Line<'static>> {
    let mut lines: Vec<Line> = Vec::new();
    if app.snapshot.components.is_empty() && app.snapshot.plugins.is_empty() {
        lines.push(Line::from(Span::styled(
            "(no components)",
            Style::new().fg(Color::DarkGray),
        )));
    }
    for component in &app.snapshot.components {
        push_boundary_component(&mut lines, component);
    }
    for component in &app.snapshot.plugins {
        push_plugin_component(&mut lines, component);
    }
    lines
}

/// One boundary [`ComponentSnapshot`] row plus its binding/health detail.
fn push_boundary_component(lines: &mut Vec<Line<'static>>, component: &ComponentSnapshot) {
    let version = component
        .api_version
        .map(|v| v.to_string())
        .unwrap_or_else(|| "-".to_string());
    let socket = match &component.socket {
        Some(name) if component.socket_present => format!("{name} up"),
        Some(name) => format!("{name} down"),
        None => "-".to_string(),
    };
    let socket_style = if component.socket_present {
        Style::new().fg(Color::Green)
    } else {
        Style::new().fg(Color::DarkGray)
    };
    lines.push(Line::from(vec![
        Span::styled(
            format!("{:<10}", component.component),
            Style::new().fg(Color::Cyan),
        ),
        Span::raw(format!("{:<10} ", component.transport)),
        Span::raw(format!("{:<16} ", format!("{}@{}", component.api, version))),
        Span::styled(socket, socket_style),
    ]));
    if !component.bindings.is_empty() {
        lines.push(Line::from(Span::styled(
            format!("    {}", component_binding_text(component)),
            Style::new().fg(Color::Gray),
        )));
    }
    if component.stalls > 0 {
        let reason = component.last_health_reason.as_deref().unwrap_or("-");
        lines.push(Line::from(Span::styled(
            format!("    health: {} stall(s) · last: {reason}", component.stalls),
            Style::new().fg(Color::Yellow),
        )));
    }
}

/// One registry/report driven plugin component and its declarative views.
fn push_plugin_component(lines: &mut Vec<Line<'static>>, component: &PluginComponent) {
    let generation = component
        .generation
        .map(|gen| gen.to_string())
        .unwrap_or_else(|| "-".to_string());
    let freshness_style = match component.freshness.as_str() {
        "fresh" => Style::new().fg(Color::Green),
        "stale" => Style::new().fg(Color::Yellow),
        "unavailable" => Style::new().fg(Color::Red),
        _ => Style::new().fg(Color::DarkGray),
    };
    let mut spans = vec![
        Span::styled(
            format!("{:<18}", component.plugin_id),
            Style::new().fg(Color::Cyan),
        ),
        Span::raw(format!("{:<10} ", component.origin)),
        Span::raw(format!("gen{generation} {:<10} ", component.lifecycle)),
        Span::styled(component.freshness.clone(), freshness_style),
    ];
    if component.stale {
        spans.push(Span::styled(
            " STALE",
            Style::new().fg(Color::Yellow).add_modifier(Modifier::BOLD),
        ));
    }
    if component.disconnected {
        spans.push(Span::styled(
            " DISCONNECTED",
            Style::new().fg(Color::Red).add_modifier(Modifier::BOLD),
        ));
    }
    if component.incomplete {
        spans.push(Span::styled(" INCOMPLETE", Style::new().fg(Color::Yellow)));
    }
    lines.push(Line::from(spans));

    if let Some(health) = &component.health {
        lines.push(Line::from(Span::styled(
            format!("    health: {} · {}", health.state, health.detail),
            Style::new().fg(Color::Gray),
        )));
    }
    if let Some(error) = &component.last_error {
        lines.push(Line::from(Span::styled(
            format!("    error: {error}"),
            Style::new().fg(Color::Red),
        )));
    }
    if !component.actions.is_empty() {
        let actions = component
            .actions
            .iter()
            .map(|action| action.action_id.clone())
            .collect::<Vec<_>>()
            .join(", ");
        lines.push(Line::from(Span::styled(
            format!("    actions: {actions}"),
            Style::new().fg(Color::Gray),
        )));
    }
    if component.views.is_empty() {
        lines.push(Line::from(Span::styled(
            "    (no declared views)",
            Style::new().fg(Color::DarkGray),
        )));
    }
    for view in &component.views {
        push_view(lines, view);
    }
}

/// One declarative view header, its state marker, note, and rendered blocks.
fn push_view(lines: &mut Vec<Line<'static>>, view: &ComponentView) {
    let (state_text, state_style) = match view.state.as_str() {
        "rendered" => (String::new(), Style::new().fg(Color::DarkGray)),
        "awaiting-data" => (
            " [awaiting-data]".to_string(),
            Style::new().fg(Color::Yellow),
        ),
        "unsupported" => (
            " [unsupported]".to_string(),
            Style::new().fg(Color::Red).add_modifier(Modifier::BOLD),
        ),
        other => (format!(" [{other}]"), Style::new().fg(Color::Yellow)),
    };
    lines.push(Line::from(vec![
        Span::styled(
            format!("  {} {}", view.kind, view.title),
            Style::new().fg(Color::Cyan),
        ),
        Span::styled(state_text, state_style),
    ]));
    if let Some(note) = &view.note {
        lines.push(Line::from(Span::styled(
            format!("    {note}"),
            Style::new().fg(Color::DarkGray),
        )));
    }
    for block in &view.blocks {
        push_block(lines, block);
    }
}

/// One generic rendered view block.
fn push_block(lines: &mut Vec<Line<'static>>, block: &ViewBlock) {
    match block {
        ViewBlock::Status { value } => lines.push(Line::from(Span::styled(
            format!("    {value}"),
            Style::new().fg(Color::Gray),
        ))),
        ViewBlock::Metrics { rows } => {
            if rows.is_empty() {
                lines.push(Line::from(Span::styled(
                    "    (no fields)",
                    Style::new().fg(Color::DarkGray),
                )));
            }
            for row in rows {
                lines.push(Line::from(vec![
                    Span::raw(format!("    {:<18} ", row.name)),
                    Span::styled(row.value.clone(), Style::new().fg(Color::Gray)),
                ]));
            }
        }
        ViewBlock::Table {
            headers,
            rows,
            truncated,
        } => {
            lines.push(Line::from(Span::styled(
                format!("    {}", headers.join(" | ")),
                Style::new().fg(Color::Gray).add_modifier(Modifier::BOLD),
            )));
            for row in rows {
                lines.push(Line::from(Span::styled(
                    format!("    {}", row.join(" | ")),
                    Style::new().fg(Color::Gray),
                )));
            }
            if *truncated > 0 {
                lines.push(Line::from(Span::styled(
                    format!("    … {truncated} more rows"),
                    Style::new().fg(Color::DarkGray),
                )));
            }
        }
        ViewBlock::Timeline { rows } => {
            for row in rows {
                lines.push(Line::from(vec![
                    Span::styled(
                        format!("    {:<14} ", row.time),
                        Style::new().fg(Color::DarkGray),
                    ),
                    Span::raw(row.text.clone()),
                ]));
            }
        }
        ViewBlock::Alert { level, message } => lines.push(Line::from(vec![
            Span::styled(
                format!("    [{}] ", level),
                Style::new().fg(Color::Yellow).add_modifier(Modifier::BOLD),
            ),
            Span::raw(message.clone()),
        ])),
        ViewBlock::ArtifactLink { label, target } => lines.push(Line::from(vec![
            Span::raw(format!("    {label} → ")),
            Span::styled(target.clone(), Style::new().fg(Color::Cyan)),
        ])),
    }
}

/// Economics pane (TUI.4): spend vs allowance, digest rollup, alerts.
fn draw_economics(frame: &mut Frame<'_>, app: &App, area: Rect) {
    let economics = &app.snapshot.economics;
    let mut lines: Vec<Line> = Vec::new();
    if economics.rows.is_empty() && economics.alerts.is_empty() {
        lines.push(Line::from(Span::styled(
            "(no cost rows)",
            Style::new().fg(Color::DarkGray),
        )));
    }
    for row in &economics.rows {
        let budget_style = if row.over_budget {
            Style::new().fg(Color::Red)
        } else {
            Style::new().fg(Color::Green)
        };
        let mut spans = vec![
            Span::styled(
                format!("{:<20} ", row.execution_id),
                Style::new().fg(Color::Cyan),
            ),
            Span::raw(format!("{:<8} ", row.phase)),
            Span::styled(row.spend_allowance_text(), budget_style),
            Span::styled(
                format!("  tok {}/{}", row.tokens_in, row.tokens_out),
                Style::new().fg(Color::DarkGray),
            ),
        ];
        if let Some(digest) = &row.digest {
            spans.push(Span::styled(
                format!("  digest: {}", digest.summary_text()),
                Style::new().fg(Color::DarkGray),
            ));
        }
        lines.push(Line::from(spans));
    }
    for alert in &economics.alerts {
        let (style, label) = match alert.level {
            crate::economics::AlertLevel::Park => (Style::new().fg(Color::Yellow), "PARKED"),
            crate::economics::AlertLevel::Degraded => (Style::new().fg(Color::Red), "DEGRADED"),
        };
        lines.push(Line::from(vec![
            Span::styled(format!("{label} "), style.add_modifier(Modifier::BOLD)),
            Span::styled(
                format!("{} ", alert.execution_id),
                Style::new().fg(Color::Cyan),
            ),
            Span::raw(alert.reason.clone()),
        ]));
    }
    let pane = Paragraph::new(Text::from(lines))
        .block(pane_block(
            Pane::Economics.title(),
            app.focus == Pane::Economics,
        ))
        .wrap(Wrap { trim: false });
    frame.render_widget(pane, area);
}

/// Timeline pane (TUI.4): curated events + digests, oldest first, tail-windowed
/// to the visible height.
fn draw_timeline(frame: &mut Frame<'_>, app: &App, area: Rect) {
    let timeline = &app.snapshot.economics.timeline;
    let inner_height = area.height.saturating_sub(2) as usize;
    let max_rows = inner_height.max(1);
    let start = timeline.len().saturating_sub(max_rows);

    let mut lines: Vec<Line> = Vec::new();
    if timeline.is_empty() {
        lines.push(Line::from(Span::styled(
            "(no events)",
            Style::new().fg(Color::DarkGray),
        )));
    } else {
        if start > 0 {
            lines.push(Line::from(Span::styled(
                format!("… {start} earlier"),
                Style::new().fg(Color::DarkGray),
            )));
        }
        for entry in &timeline[start..] {
            let sequence = if entry.kind == "digest" {
                "·".to_string()
            } else {
                format!("{}", entry.sequence)
            };
            lines.push(Line::from(vec![
                Span::styled(format!("{sequence:>3} "), Style::new().fg(Color::DarkGray)),
                Span::styled(format!("{:<14} ", entry.kind), Style::new().fg(Color::Cyan)),
                Span::styled(
                    format!("{:<18} ", entry.execution_id),
                    Style::new().fg(Color::Gray),
                ),
                Span::raw(entry.summary.clone()),
            ]));
        }
    }
    let pane = Paragraph::new(Text::from(lines))
        .block(pane_block(
            Pane::Timeline.title(),
            app.focus == Pane::Timeline,
        ))
        .wrap(Wrap { trim: false });
    frame.render_widget(pane, area);
}

/// Footer: keybindings on one line, the current mode/status on the next.
fn draw_footer(frame: &mut Frame<'_>, app: &App, area: Rect) {
    let keys = "q quit   r refresh   Tab/1-4 focus   j/k select   [ ] scroll   p pause   R resume   c cancel";
    let mut lines = vec![Line::from(Span::styled(keys, Style::new().fg(Color::Gray)))];

    match &app.mode {
        Mode::Confirm { action, input } => {
            lines.push(Line::from(vec![
                Span::styled(
                    format!(
                        "confirm {}: type \"{}\" then Enter (Esc cancels) > ",
                        action.name(),
                        action.name()
                    ),
                    Style::new().fg(Color::Yellow).add_modifier(Modifier::BOLD),
                ),
                Span::raw(input.clone()),
            ]));
        }
        Mode::Normal => {
            let mut spans = Vec::new();
            if let Some(status) = &app.status {
                spans.push(Span::styled(status.clone(), Style::new().fg(Color::Cyan)));
                spans.push(Span::raw("   "));
            }
            if let Some(selected) = app.snapshot.executions.get(app.selection) {
                spans.push(Span::styled(
                    format!("sel {}", selected.execution_id),
                    Style::new().fg(Color::DarkGray),
                ));
            }
            lines.push(Line::from(spans));
        }
    }
    frame.render_widget(Paragraph::new(Text::from(lines)), area);
}

/// Style for an execution state cell.
fn state_style(state: &str) -> Style {
    match state {
        "Running" | "Dispatched" | "Completed" | "Done" => Style::new().fg(Color::Green),
        "Parked" => Style::new().fg(Color::Yellow),
        "Failed" => Style::new().fg(Color::Red),
        _ => Style::new().fg(Color::Cyan),
    }
}

/// `N` plus the first pending operation id, or `-` when there are none.
fn pending_text(count: usize, operations: &[String]) -> String {
    match operations.first() {
        Some(first) if count == 1 => first.clone(),
        Some(first) => format!("{count} ({first})"),
        None => "-".to_string(),
    }
}

/// One-line binding summary for a component (`impl@ver State genN [caps] pins:N`).
fn component_binding_text(component: &crate::components::ComponentSnapshot) -> String {
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

/// Flatten a rendered buffer into visible text (per-line trailing space
/// trimmed).
fn buffer_text(buffer: &Buffer) -> String {
    let area = buffer.area();
    let mut out = String::new();
    for y in 0..area.height {
        let mut line = String::new();
        for x in 0..area.width {
            line.push_str(buffer[(x, y)].symbol());
        }
        out.push_str(line.trim_end());
        if y + 1 < area.height {
            out.push('\n');
        }
    }
    out
}

/// Visible terminal size `(columns, rows)`.
///
/// Prefers `stty size`, then `COLUMNS`/`LINES`, then `80x24`. Read-only: spawns
/// `stty` at most, never writes.
fn terminal_size() -> (u16, u16) {
    if let Ok(out) = std::process::Command::new("stty").arg("size").output() {
        if out.status.success() {
            let text = String::from_utf8_lossy(&out.stdout);
            let mut parts = text.split_whitespace();
            if let (Some(rows), Some(cols)) = (parts.next(), parts.next()) {
                if let (Ok(rows), Ok(cols)) = (rows.parse::<u16>(), cols.parse::<u16>()) {
                    if rows > 0 && cols > 0 {
                        return (cols, rows);
                    }
                }
            }
        }
    }
    let env_size = (|| {
        let cols = std::env::var("COLUMNS").ok()?.trim().parse::<u16>().ok()?;
        let rows = std::env::var("LINES").ok()?.trim().parse::<u16>().ok()?;
        Some((cols, rows))
    })();
    env_size
        .filter(|(c, r)| *c > 0 && *r > 0)
        .unwrap_or((80, 24))
}

#[cfg(test)]
mod tests {
    use super::*;

    use std::cell::Cell;
    use std::rc::Rc;

    use agalma_contracts::{ExecutionApi, LedgerApi, TaskId};
    use agalma_testkit::ScratchDir;

    use crate::components::{BindingView, ComponentSnapshot};
    use crate::control::ControlOutcome;
    use crate::economics::{
        Alert, AlertLevel, DigestSummary, EconomicsRow, EconomicsSnapshot, TimelineEntry,
    };
    use crate::status::{ExecutionSnapshot, Snapshot};

    fn ctx_transports() -> Transports {
        Transports::all_in_process()
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
            plugins: Vec::new(),
            economics: EconomicsSnapshot::default(),
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

    /// One running execution with two pending operations.
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
            plugins: Vec::new(),
            economics: economics(),
        }
    }

    /// One execution's economics row, a budget-exceeded row, timeline entries,
    /// and both alert levels.
    fn economics() -> EconomicsSnapshot {
        EconomicsSnapshot {
            rows: vec![
                EconomicsRow {
                    execution_id: "exec:task-a:1".to_string(),
                    task_id: "task-a".to_string(),
                    phase: "Build".to_string(),
                    state: "Running".to_string(),
                    attempt: 2,
                    tokens_in: 1500,
                    tokens_out: 320,
                    spend_usd: 0.25,
                    allowance_usd: Some(2.0),
                    remaining_usd: Some(1.75),
                    over_budget: false,
                    stalls: 0,
                    degraded: false,
                    parked: false,
                    parked_reason: None,
                    digest: Some(DigestSummary {
                        verdict: "green".to_string(),
                        attempts: 2,
                        tokens_in: 1500,
                        tokens_out: 320,
                        usd: 0.25,
                        wall_ms: 4200,
                        stalls: 0,
                    }),
                },
                EconomicsRow {
                    execution_id: "exec:task-b:1".to_string(),
                    task_id: "task-b".to_string(),
                    phase: "Verify".to_string(),
                    state: "Parked".to_string(),
                    attempt: 1,
                    tokens_in: 900,
                    tokens_out: 100,
                    spend_usd: 0.30,
                    allowance_usd: Some(0.20),
                    remaining_usd: Some(-0.10),
                    over_budget: true,
                    stalls: 2,
                    degraded: true,
                    parked: true,
                    parked_reason: Some("session_gone".to_string()),
                    digest: None,
                },
            ],
            timeline: vec![
                TimelineEntry {
                    execution_id: "exec:task-a:1".to_string(),
                    sequence: 1,
                    kind: "claim".to_string(),
                    summary: "claimed lease 3".to_string(),
                    recorded_at_unix_ms: 10,
                },
                TimelineEntry {
                    execution_id: "exec:task-a:1".to_string(),
                    sequence: 2,
                    kind: "decision".to_string(),
                    summary: "decision recorded op:build@2@2".to_string(),
                    recorded_at_unix_ms: 20,
                },
                TimelineEntry {
                    execution_id: "exec:task-b:1".to_string(),
                    sequence: 4,
                    kind: "provider_health".to_string(),
                    summary: "degraded: session_gone".to_string(),
                    recorded_at_unix_ms: 30,
                },
                TimelineEntry {
                    execution_id: "exec:task-a:1".to_string(),
                    sequence: 0,
                    kind: "digest".to_string(),
                    summary: "green attempts=2 tokens=1500/320 usd=0.250000 wall_ms=4200 stalls=0"
                        .to_string(),
                    recorded_at_unix_ms: 40,
                },
            ],
            alerts: vec![
                Alert {
                    execution_id: "exec:task-b:1".to_string(),
                    level: AlertLevel::Park,
                    reason: "session_gone".to_string(),
                },
                Alert {
                    execution_id: "exec:task-b:1".to_string(),
                    level: AlertLevel::Degraded,
                    reason: "2 stall(s) · last: session_gone".to_string(),
                },
            ],
        }
    }

    fn app(snapshot: Snapshot) -> App {
        App::new(snapshot, ctx_transports(), 1000)
    }

    fn screen(app: &App) -> String {
        render_to_string(app, 150, 40)
    }

    fn press(code: KeyCode) -> KeyEvent {
        KeyEvent::new(code, KeyModifiers::NONE)
    }

    /// Records every control request and returns a fixed outcome.
    struct RecordingDriver {
        calls: Vec<ControlRequest>,
        outcome: ControlOutcome,
    }

    impl RecordingDriver {
        fn applied() -> Self {
            RecordingDriver {
                calls: Vec::new(),
                outcome: ControlOutcome::Applied,
            }
        }

        fn denied(reason: &str) -> Self {
            RecordingDriver {
                calls: Vec::new(),
                outcome: ControlOutcome::Denied {
                    reason: reason.to_string(),
                },
            }
        }
    }

    impl ControlDriver for RecordingDriver {
        fn apply(&mut self, request: &ControlRequest) -> Result<ControlOutcome, ContractError> {
            self.calls.push(request.clone());
            Ok(self.outcome.clone())
        }
    }

    #[test]
    fn absent_snapshot_renders_exists_false_and_no_rows() {
        let app = app(absent());
        let plain = screen(&app);
        assert!(plain.contains("exists:false"), "{plain}");
        assert!(plain.contains("binding: -"), "{plain}");
        assert!(plain.contains("kill_latch: false"), "{plain}");
        // Every pane is present even when empty.
        for pane in Pane::ALL {
            assert!(
                plain.contains(pane.title()),
                "missing pane {}: {plain}",
                pane.title()
            );
        }
        assert!(
            plain.contains("(no executions)") || plain.contains("EXECUTION"),
            "{plain}"
        );
        assert!(plain.contains("(no components)"), "{plain}");
        assert!(plain.contains("(no cost rows)"), "{plain}");
        assert!(plain.contains("(no events)"), "{plain}");
    }

    #[test]
    fn present_snapshot_renders_all_panes() {
        let app = app(present());
        let plain = screen(&app);
        assert!(plain.contains("exists:true"), "{plain}");
        assert!(plain.contains("kill_latch: true"), "{plain}");
        assert!(plain.contains("binding: reference@reference/v1"), "{plain}");
        assert!(plain.contains("ledger: in-process"), "{plain}");
        assert!(plain.contains("harness: in-process"), "{plain}");
        assert!(plain.contains("interval: 1000 ms"), "{plain}");
        // Executions pane.
        assert!(plain.contains("exec:task-a:1"), "{plain}");
        assert!(plain.contains("Build"), "{plain}");
        assert!(plain.contains("Running"), "{plain}");
        // Components pane.
        assert!(plain.contains("harness*.sock up"), "{plain}");
        assert!(plain.contains("ledger.sock down"), "{plain}");
        assert!(
            plain.contains("reference@reference/v1 Active gen1"),
            "{plain}"
        );
        assert!(plain.contains("[context_hooks]"), "{plain}");
        assert!(plain.contains("pins:1"), "{plain}");
        assert!(plain.contains("2 stall(s)"), "{plain}");
        assert!(plain.contains("last: session_gone"), "{plain}");
        // Economics pane.
        assert!(plain.contains("$0.250000 / $2.00 rem $1.750000"), "{plain}");
        assert!(
            plain.contains("$0.300000 / $0.20 rem $-0.100000 OVER"),
            "{plain}"
        );
        assert!(plain.contains("tok 1500/320"), "{plain}");
        assert!(plain.contains("green attempts=2"), "{plain}");
        assert!(plain.contains("PARKED"), "{plain}");
        assert!(plain.contains("DEGRADED"), "{plain}");
        // Timeline pane, oldest first (lines may wrap, so assert on stable
        // fragments and their relative order).
        assert!(plain.contains("claimed lease 3"), "{plain}");
        assert!(plain.contains("decision recorded"), "{plain}");
        assert!(plain.contains("degraded: session_gone"), "{plain}");
        assert!(plain.contains("· digest"), "{plain}");
        let claim = plain.find("claimed lease 3").expect("claim");
        let decision = plain.find("decision recorded").expect("decision");
        let digest = plain.find("· digest").expect("digest milestone");
        assert!(
            claim < decision && decision < digest,
            "timeline order: {plain}"
        );
    }

    #[test]
    fn focus_marker_follows_focus_model() {
        let mut app = app(present());
        let mut driver = RecordingDriver::applied();

        assert!(screen(&app).contains("▶ Executions"), "{}", screen(&app));

        app.handle_key(press(KeyCode::Tab), &mut driver);
        assert_eq!(app.focus, Pane::Components);
        assert!(screen(&app).contains("▶ Components"));

        app.handle_key(press(KeyCode::Tab), &mut driver);
        assert_eq!(app.focus, Pane::Economics);
        app.handle_key(press(KeyCode::Tab), &mut driver);
        assert_eq!(app.focus, Pane::Timeline);
        app.handle_key(press(KeyCode::Tab), &mut driver);
        assert_eq!(app.focus, Pane::Executions);

        // Shift-Tab walks backwards.
        app.handle_key(
            KeyEvent::new(KeyCode::BackTab, KeyModifiers::SHIFT),
            &mut driver,
        );
        assert_eq!(app.focus, Pane::Timeline);

        // Number keys jump directly.
        app.handle_key(press(KeyCode::Char('2')), &mut driver);
        assert_eq!(app.focus, Pane::Components);
        app.handle_key(press(KeyCode::Char('3')), &mut driver);
        assert_eq!(app.focus, Pane::Economics);
    }

    #[test]
    fn components_window_covers_every_row_at_every_boundary() {
        // Everything fits: the whole list renders, with no marker and no
        // vanished trailing line (the previous boundary bug).
        for total in 1..=5usize {
            let window = components_window(total, 0, 5);
            assert_eq!((window.start, window.end), (0, total), "total={total}");
            assert!(!window.earlier && !window.more, "total={total}");
        }
        // Exactly one row over the pane: the last row stays reachable.
        let window = components_window(6, 5, 5);
        assert_eq!(window.start, 3);
        assert_eq!(window.end, 6);
        assert!(window.earlier && !window.more);

        // Exhaustive reachability: every index appears in some window, and no
        // window exceeds the pane height.
        for max_rows in 1..=8usize {
            for total in 1..=20usize {
                let mut seen = vec![false; total];
                for offset in 0..total {
                    let window = components_window(total, offset, max_rows);
                    assert!(
                        window.end <= total,
                        "window escaped: total={total} offset={offset} max_rows={max_rows}"
                    );
                    let markers = usize::from(window.earlier) + usize::from(window.more);
                    let shown = window.end - window.start;
                    if max_rows >= 3 {
                        assert!(
                            shown + markers <= max_rows,
                            "window overflows: total={total} offset={offset} max_rows={max_rows} \
                             shown={shown} markers={markers}"
                        );
                    }
                    for index in window.start..window.end {
                        seen[index] = true;
                    }
                }
                assert!(
                    seen.iter().all(|covered| *covered),
                    "unreachable row: total={total} max_rows={max_rows}"
                );
            }
        }
    }

    #[test]
    fn components_scroll_keybindings_are_bounded() {
        let mut app = app(present());
        let mut driver = RecordingDriver::applied();
        let total = component_line_count(&app);
        assert!(total > 1, "fixture has component rows: {total}");

        app.handle_key(press(KeyCode::Char(']')), &mut driver);
        assert_eq!(app.components_offset, 1);
        app.handle_key(press(KeyCode::Char('[')), &mut driver);
        assert_eq!(app.components_offset, 0);

        app.handle_key(press(KeyCode::PageDown), &mut driver);
        assert!(app.components_offset <= total - 1);
        app.handle_key(press(KeyCode::PageUp), &mut driver);
        assert_eq!(app.components_offset, 0);
    }

    #[test]
    fn quit_and_refresh_keybindings() {
        let mut app = app(present());
        let mut driver = RecordingDriver::applied();

        app.handle_key(press(KeyCode::Char('r')), &mut driver);
        assert!(app.refresh_requested, "r requests a refresh");

        app.handle_key(press(KeyCode::Char('q')), &mut driver);
        assert!(app.quit, "q quits");

        app.quit = false;
        app.handle_key(
            KeyEvent::new(KeyCode::Char('c'), KeyModifiers::CONTROL),
            &mut driver,
        );
        assert!(app.quit, "Ctrl-C quits");
    }

    #[test]
    fn selection_moves_with_j_k_and_arrows() {
        let mut snapshot = present();
        snapshot.executions = (0..3)
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
        let mut app = app(snapshot);
        let mut driver = RecordingDriver::applied();

        app.handle_key(press(KeyCode::Char('j')), &mut driver);
        assert_eq!(app.selection, 1);
        app.handle_key(press(KeyCode::Down), &mut driver);
        assert_eq!(app.selection, 2);
        // Saturates at the end.
        app.handle_key(press(KeyCode::Char('j')), &mut driver);
        assert_eq!(app.selection, 2);
        app.handle_key(press(KeyCode::Char('k')), &mut driver);
        assert_eq!(app.selection, 1);
        app.handle_key(press(KeyCode::Up), &mut driver);
        assert_eq!(app.selection, 0);
        app.handle_key(press(KeyCode::Char('k')), &mut driver);
        assert_eq!(app.selection, 0);
    }

    #[test]
    fn pause_applies_without_confirmation() {
        let mut app = app(present());
        let mut driver = RecordingDriver::applied();

        app.handle_key(press(KeyCode::Char('p')), &mut driver);

        assert_eq!(driver.calls.len(), 1, "one apply");
        assert_eq!(driver.calls[0].action, ControlAction::Pause);
        assert_eq!(driver.calls[0].actor, Actor::Human);
        assert!(driver.calls[0].confirmation.is_none(), "pause needs none");
        assert!(app
            .status
            .as_deref()
            .unwrap_or("")
            .contains("pause: applied"));
        assert!(app.refresh_requested, "control effect refreshes the view");
        assert_eq!(app.mode, Mode::Normal);
    }

    #[test]
    fn resume_requires_typed_confirmation() {
        let mut app = app(present());
        let mut driver = RecordingDriver::applied();

        app.handle_key(press(KeyCode::Char('R')), &mut driver);
        assert!(matches!(app.mode, Mode::Confirm { .. }));
        assert!(driver.calls.is_empty(), "not applied before confirmation");
        let confirm_screen = screen(&app);
        assert!(
            confirm_screen.contains("type \"resume\""),
            "{confirm_screen}"
        );

        // Wrong phrase is rejected and stays in confirmation mode.
        for c in "resumx".chars() {
            app.handle_key(press(KeyCode::Char(c)), &mut driver);
        }
        app.handle_key(press(KeyCode::Enter), &mut driver);
        assert!(matches!(app.mode, Mode::Confirm { .. }));
        assert!(driver.calls.is_empty(), "mismatch does not apply");
        assert!(app
            .status
            .as_deref()
            .unwrap_or("")
            .contains("confirmation mismatch"));

        // Correct phrase applies with the typed confirmation recorded.
        app.handle_key(press(KeyCode::Backspace), &mut driver);
        app.handle_key(press(KeyCode::Char('e')), &mut driver);
        app.handle_key(press(KeyCode::Enter), &mut driver);
        assert_eq!(driver.calls.len(), 1, "applied once");
        assert_eq!(driver.calls[0].action, ControlAction::Resume);
        assert_eq!(driver.calls[0].confirmation.as_deref(), Some("resume"));
        assert_eq!(app.mode, Mode::Normal);
    }

    #[test]
    fn escape_cancels_confirmation() {
        let mut app = app(present());
        let mut driver = RecordingDriver::applied();
        app.handle_key(press(KeyCode::Char('R')), &mut driver);
        app.handle_key(press(KeyCode::Char('r')), &mut driver);
        app.handle_key(press(KeyCode::Esc), &mut driver);
        assert_eq!(app.mode, Mode::Normal);
        assert!(driver.calls.is_empty());
        assert!(app
            .status
            .as_deref()
            .unwrap_or("")
            .contains("confirmation cancelled"));
    }

    #[test]
    fn cancel_selects_execution_and_requires_confirmation() {
        let mut app = app(present());
        let mut driver = RecordingDriver::applied();

        app.handle_key(press(KeyCode::Char('c')), &mut driver);
        assert!(matches!(app.mode, Mode::Confirm { .. }));
        assert!(screen(&app).contains("type \"cancel\""));
        for c in "cancel".chars() {
            app.handle_key(press(KeyCode::Char(c)), &mut driver);
        }
        app.handle_key(press(KeyCode::Enter), &mut driver);

        assert_eq!(driver.calls.len(), 1);
        assert_eq!(
            driver.calls[0].action,
            ControlAction::Cancel {
                execution: ExecutionId::new("exec:task-a:1"),
            }
        );
        assert_eq!(driver.calls[0].confirmation.as_deref(), Some("cancel"));
    }

    #[test]
    fn cancel_with_no_execution_is_refused() {
        let mut app = app(absent());
        let mut driver = RecordingDriver::applied();
        app.handle_key(press(KeyCode::Char('c')), &mut driver);
        assert_eq!(app.mode, Mode::Normal);
        assert!(driver.calls.is_empty());
        assert!(app
            .status
            .as_deref()
            .unwrap_or("")
            .contains("no execution selected"));
    }

    #[test]
    fn denial_is_surfaced_in_status() {
        let mut app = app(present());
        let mut driver = RecordingDriver::denied("already paused (kill latch set)");
        app.handle_key(press(KeyCode::Char('p')), &mut driver);
        let status = app.status.as_deref().unwrap_or("");
        assert!(status.contains("pause: denied"), "{status}");
        assert!(status.contains("already paused"), "{status}");
    }

    #[test]
    fn once_frame_renders_single_frame_without_writing() {
        let dir = ScratchDir::new("watch-absent-once");
        let frame = once_frame(dir.path(), 1000).expect("once frame");
        assert!(frame.contains("exists:false"), "{frame}");
        assert!(frame.contains("agalma watch"), "{frame}");

        // Acceptance: strictly no writes — the state dir stays empty.
        let entries: Vec<_> = std::fs::read_dir(dir.path())
            .expect("read dir")
            .map(|e| e.expect("entry").file_name())
            .collect();
        assert!(entries.is_empty(), "watch wrote to state dir: {entries:?}");
    }

    #[test]
    fn test_backend_frame_is_exactly_sized() {
        let app = app(present());
        let text = render_to_string(&app, 120, 30);
        let lines: Vec<&str> = text.split('\n').collect();
        assert_eq!(lines.len(), 30, "one line per buffer row");
        for line in lines {
            assert!(
                line.chars().count() <= 120,
                "line wider than buffer: {line:?}"
            );
        }
        // Narrow frames must not panic.
        let _ = render_to_string(&app, 40, 12);
    }

    // ---- Terminal lifecycle -------------------------------------------------

    #[derive(Clone, Default)]
    struct RecordingMode {
        entered: Rc<Cell<usize>>,
        exited: Rc<Cell<usize>>,
        fail_enter: bool,
    }

    impl TerminalMode for RecordingMode {
        fn enter(&mut self) -> io::Result<()> {
            if self.fail_enter {
                return Err(io::Error::other("enter failed"));
            }
            self.entered.set(self.entered.get() + 1);
            Ok(())
        }

        fn exit(&mut self) -> io::Result<()> {
            self.exited.set(self.exited.get() + 1);
            Ok(())
        }
    }

    #[test]
    fn mode_guard_restores_on_drop() {
        let mode = RecordingMode::default();
        {
            let _guard = ModeGuard::enter(mode.clone()).expect("enter");
            assert_eq!(mode.entered.get(), 1);
            assert_eq!(mode.exited.get(), 0);
        }
        assert_eq!(
            mode.exited.get(),
            1,
            "drop restores the terminal exactly once"
        );
    }

    #[test]
    fn mode_guard_restores_on_panic_unwind() {
        let mode = RecordingMode::default();
        let inside = mode.clone();
        let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            let _guard = ModeGuard::enter(inside).expect("enter");
            panic!("boom");
        }));
        assert!(result.is_err(), "the panic propagates");
        assert_eq!(mode.exited.get(), 1, "unwind restores the terminal");
    }

    #[test]
    fn mode_guard_enter_failure_does_not_restore() {
        let mode = RecordingMode {
            fail_enter: true,
            ..RecordingMode::default()
        };
        assert!(ModeGuard::enter(mode.clone()).is_err());
        assert_eq!(mode.entered.get(), 0);
        assert_eq!(mode.exited.get(), 0, "nothing entered, nothing to restore");
    }

    // ---- Control wired through the real ControlPlane ----------------------

    fn scratch_executor(dir: &ScratchDir) -> agalma_execution::Executor<SqliteLedger> {
        let ledger = SqliteLedger::open(dir.join("ledger.sqlite")).expect("open ledger");
        agalma_execution::Executor::new(
            ledger,
            ExecutorConfig {
                max_attempts: 1,
                execution_definition_version: EXECUTION_DEFINITION_VERSION,
            },
        )
    }

    #[test]
    fn ledger_driver_pause_writes_latch_and_receipt() {
        let dir = ScratchDir::new("watch-control-pause");
        let mut ex = scratch_executor(&dir);
        let _ = ex.start(&TaskId::new("watch")).expect("start execution");
        drop(ex);

        let snapshot = crate::status::snapshot_with_selected_transport(
            dir.path(),
            &ctx_transports(),
            crate::ledger_process::LedgerTransport::InProcess,
        )
        .expect("snapshot");
        let mut app = App::new(snapshot, ctx_transports(), 1000);
        let mut driver = LedgerControlDriver::new(dir.path());

        app.handle_key(press(KeyCode::Char('p')), &mut driver);
        assert!(app
            .status
            .as_deref()
            .unwrap_or("")
            .contains("pause: applied"));

        let ledger = SqliteLedger::open(dir.join("ledger.sqlite")).expect("reopen ledger");
        assert!(ledger.kill_latch().expect("latch"), "pause latched");
        let receipts = ledger.control_receipts().expect("receipts");
        assert_eq!(receipts.len(), 1, "one durable receipt: {receipts:?}");
        assert_eq!(receipts[0].action, "pause");
        assert_eq!(receipts[0].actor, "human");
        assert_eq!(receipts[0].request["outcome"], "applied");
    }

    #[test]
    fn ledger_driver_cancel_confirmation_receipts_and_requests_cancel() {
        let dir = ScratchDir::new("watch-control-cancel");
        let mut ex = scratch_executor(&dir);
        let execution = ex.start(&TaskId::new("watch")).expect("start execution");
        drop(ex);

        let snapshot = crate::status::snapshot_with_selected_transport(
            dir.path(),
            &ctx_transports(),
            crate::ledger_process::LedgerTransport::InProcess,
        )
        .expect("snapshot");
        let mut app = App::new(snapshot, ctx_transports(), 1000);
        let mut driver = LedgerControlDriver::new(dir.path());

        app.handle_key(press(KeyCode::Char('c')), &mut driver);
        for c in "cancel".chars() {
            app.handle_key(press(KeyCode::Char(c)), &mut driver);
        }
        app.handle_key(press(KeyCode::Enter), &mut driver);
        assert!(app
            .status
            .as_deref()
            .unwrap_or("")
            .contains("cancel: applied"));

        let ledger = SqliteLedger::open(dir.join("ledger.sqlite")).expect("reopen ledger");
        assert!(
            !ledger.kill_latch().expect("latch"),
            "cancel must not set the global pause latch"
        );
        let events = ledger.events(&execution).expect("events");
        assert!(
            events.iter().any(|e| e.kind == "cancel_requested"),
            "cancel_requested recorded: {:?}",
            events.iter().map(|e| &e.kind).collect::<Vec<_>>()
        );
        let receipts = ledger.control_receipts().expect("receipts");
        assert_eq!(receipts.len(), 1, "one receipt: {receipts:?}");
        assert_eq!(receipts[0].action, "cancel");
        assert_eq!(receipts[0].target, execution.to_string());
    }

    #[test]
    fn pending_text_summarizes_operations() {
        assert_eq!(pending_text(0, &[]), "-");
        assert_eq!(pending_text(1, &["op:build@2".to_string()]), "op:build@2");
        assert_eq!(
            pending_text(2, &["op:build@2".to_string(), "op:build@3".to_string()]),
            "2 (op:build@2)"
        );
    }
}
