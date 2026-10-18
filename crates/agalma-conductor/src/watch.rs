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

use std::collections::BTreeMap;
use std::io::{self, IsTerminal, Write};
use std::path::{Path, PathBuf};
use std::process::ExitCode;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
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

use agalma_contracts::digest::FailureCode;
use agalma_contracts::{ActionScope, ContractError, ExecutionId};
use agalma_execution::{Executor, ExecutorConfig, EXECUTION_DEFINITION_VERSION};
use agalma_ledger::SqliteLedger;

use crate::cli::WatchArgs;
use crate::components::{
    ActionView, ComponentSnapshot, ComponentView, PluginComponent, Transports, ViewBlock,
};
use crate::control::{
    new_action_id, Actor, ControlAction, ControlOutcome, ControlPlane, ControlRequest,
    PluginActionControl,
};
use crate::plugin_actions::{ActionOutcome, ActionRequest, ActionService};
use crate::plugin_router::LaunchGrant;
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

/// One validated plugin-action invocation requested from the UI.
///
/// The UI carries no plugin authority of its own: the plugin id, pinned
/// generation, advertised action id, typed input, and (for a
/// confirmation-required action) the typed phrase are all it supplies. The
/// common action service performs every grant/capability/binding/deadline and
/// human-confirmation check.
#[derive(Clone, Debug, PartialEq)]
pub struct ActionInvocation {
    pub plugin_id: String,
    pub generation: u64,
    pub action_id: String,
    pub input: serde_json::Value,
    pub confirmation: Option<String>,
}

/// Applies one plugin-action invocation (production: the hub's common action
/// service; tests: a recording double), parallel to [`ControlDriver`].
pub trait ActionDriver {
    fn dispatch(&mut self, invocation: &ActionInvocation) -> Result<ActionOutcome, ContractError>;
}

/// Default [`ActionDriver`]: no plugin action service is attached. Every
/// invocation is refused with [`ContractError::UnsupportedCapability`] — the UI
/// never invents action authority. Production grant issuance and the pinned
/// plugin RPC executor are attached by the host (HP.8).
pub struct UnavailableActionDriver;

impl ActionDriver for UnavailableActionDriver {
    fn dispatch(&mut self, _invocation: &ActionInvocation) -> Result<ActionOutcome, ContractError> {
        Err(ContractError::UnsupportedCapability(
            "no plugin action service is attached".to_string(),
        ))
    }
}

/// The production [`ActionDriver`]: routes a UI invocation through the hub's
/// common validated [`ActionService`] via [`PluginActionControl`].
///
/// The UI never mints authority: the host supplies the parent-issued
/// [`LaunchGrant`] per plugin through a shared grant source, and the driver
/// re-reads it before **every** dispatch. On a host restart the launch grant
/// rotates; [`Self::sync_grants`] installs the current grant and revokes the
/// stale fingerprint from the active service, so a stale token is denied and the
/// new launch grant works. A missing grant is an [`FailureCode::AuthorityDenied`]
/// with zero effects. Every request carries a fresh, restart-unique
/// `ui:{action_id}:{now_ms}:{n}` operation id, a deadline derived from the
/// injected clock, and an [`ActionScope`] pinned to this driver's directive id.
pub struct ServiceActionDriver {
    service: ActionService,
    /// Host-owned current grants, re-read before every dispatch.
    grants: Arc<Mutex<BTreeMap<String, LaunchGrant>>>,
    /// Fingerprint currently installed in the service per plugin (to revoke a
    /// stale one when the host rotates the launch grant).
    installed: BTreeMap<String, String>,
    now: Arc<dyn Fn() -> u64 + Send + Sync>,
    next_op: u64,
    directive_id: String,
    default_deadline_ms: u64,
}

impl ServiceActionDriver {
    /// Build the driver over the hub's common action service with a private,
    /// empty grant source. Defaults: a system clock, a 60 s deadline, and
    /// directive `directive/ui`.
    pub fn new(service: ActionService) -> Self {
        Self::with_grants(service, Arc::new(Mutex::new(BTreeMap::new())))
    }

    /// Build the driver over the hub's common action service and a host-shared
    /// grant source (re-read before every dispatch, so a restart's rotated grant
    /// replaces/revokes the stale one in the active service).
    pub fn with_grants(
        service: ActionService,
        grants: Arc<Mutex<BTreeMap<String, LaunchGrant>>>,
    ) -> Self {
        ServiceActionDriver {
            service,
            grants,
            installed: BTreeMap::new(),
            now: Arc::new(system_now_ms),
            next_op: 0,
            directive_id: "directive/ui".to_string(),
            default_deadline_ms: 60_000,
        }
    }

    /// Install (or replace) a parent-issued per-launch grant for its plugin in
    /// the shared grant source. A plugin with no installed grant is refused with
    /// zero effects; the service table is synced on the next dispatch.
    pub fn install_grant(&mut self, grant: &LaunchGrant) {
        self.grants
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .insert(grant.scope().plugin_id.clone(), grant.clone());
    }

    /// Sync the active service's grant table to the shared current grants:
    /// install each current grant and revoke a previously-installed fingerprint
    /// that has rotated. Returns the revoked fingerprints. Called before every
    /// dispatch so a stale launch grant can never authorize a further action.
    pub fn sync_grants(&mut self) -> Vec<String> {
        let current: BTreeMap<String, LaunchGrant> = self
            .grants
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .clone();
        let mut revoked = Vec::new();
        for (plugin_id, grant) in &current {
            let fingerprint = grant.fingerprint();
            if let Some(previous) = self.installed.get(plugin_id) {
                if *previous != fingerprint {
                    self.service.revoke_grant(previous);
                    revoked.push(previous.clone());
                }
            }
            self.service.install_grant(grant);
            self.installed.insert(plugin_id.clone(), fingerprint);
        }
        revoked
    }

    /// Override the clock (deterministic tests).
    pub fn with_clock(mut self, now: Arc<dyn Fn() -> u64 + Send + Sync>) -> Self {
        self.now = now;
        self
    }

    /// Override the default action deadline (milliseconds).
    pub fn with_deadline_ms(mut self, default_deadline_ms: u64) -> Self {
        self.default_deadline_ms = default_deadline_ms;
        self
    }

    /// The active validated service (host/test seam): the exact table a dispatch
    /// uses, so a host caller/test can observe grant revocation directly.
    pub fn service_mut(&mut self) -> &mut ActionService {
        &mut self.service
    }
}

impl ActionDriver for ServiceActionDriver {
    fn dispatch(&mut self, invocation: &ActionInvocation) -> Result<ActionOutcome, ContractError> {
        // Re-read the host's current grants and revoke any rotated one before
        // presenting a token.
        self.sync_grants();
        let grant = self
            .grants
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .get(&invocation.plugin_id)
            .map(|grant| grant.token_hex());
        let Some(grant) = grant else {
            return Err(FailureCode::AuthorityDenied.failure(
                &invocation.plugin_id,
                "no parent-issued grant is installed for this plugin",
            ));
        };
        self.next_op += 1;
        let now = (self.now)();
        // Restart-unique: the injected clock plus a per-session counter, so a
        // fresh UI session never collides with (and silently dedupes) an earlier
        // session's operation.
        let operation_id = format!("ui:{}:{}:{}", invocation.action_id, now, self.next_op);
        let request = ActionRequest {
            grant,
            plugin_id: invocation.plugin_id.clone(),
            generation: invocation.generation,
            action_id: invocation.action_id.clone(),
            operation_id,
            input: invocation.input.clone(),
            deadline_unix_ms: now + self.default_deadline_ms,
            actor: agalma_contracts::Actor::Human,
            confirmation: invocation.confirmation.clone(),
            scope: ActionScope {
                directive_id: self.directive_id.clone(),
                directive_version: 1,
                execution_id: None,
            },
        };
        let mut control = PluginActionControl::new(&mut self.service);
        control.dispatch(&request)
    }
}

/// Build the production [`ActionDriver`] from the hub's common [`ActionService`]
/// and the parent-issued grants the host installed.
///
/// This is the seam the production host uses to attach real plugin actions to
/// the interactive view: it owns grant issuance and the pinned-plugin RPC
/// `ActionExecutor`, builds the service, and passes the grants here. Each grant
/// is registered hub-side (so the service authenticates the presented token) and
/// supplies the presented token. The returned box is `Send`, so the host can move
/// it into [`run_with_actions`].
pub fn service_action_driver(
    service: ActionService,
    grants: &[LaunchGrant],
) -> Box<dyn ActionDriver + Send> {
    service_action_driver_with_clock(service, grants, Arc::new(system_now_ms))
}

/// [`service_action_driver`] with an injected clock (deterministic tests); the
/// production host uses the system-clock default.
pub fn service_action_driver_with_clock(
    service: ActionService,
    grants: &[LaunchGrant],
    now: Arc<dyn Fn() -> u64 + Send + Sync>,
) -> Box<dyn ActionDriver + Send> {
    let mut driver = ServiceActionDriver::new(service).with_clock(now);
    for grant in grants {
        driver.install_grant(grant);
    }
    Box::new(driver)
}

/// [`service_action_driver`] over a host-**shared** grant source that the host
/// refreshes on every launch/restart tick. The driver re-reads it before each
/// dispatch and revokes a rotated grant from the active service, so a stale
/// launch grant is denied and the new one works.
pub fn service_action_driver_shared(
    service: ActionService,
    grants: Arc<Mutex<BTreeMap<String, LaunchGrant>>>,
) -> Box<dyn ActionDriver + Send> {
    Box::new(ServiceActionDriver::with_grants(service, grants))
}

/// Resolve the interactive loop's plugin-action driver: the host-injected
/// production driver when present, else the fail-closed default
/// ([`UnavailableActionDriver`]). The loop binds exactly this driver, so a
/// missing service can never invent action authority.
pub fn bind_action_driver(injected: Option<Box<dyn ActionDriver + Send>>) -> Box<dyn ActionDriver> {
    match injected {
        Some(driver) => driver,
        None => Box::new(UnavailableActionDriver),
    }
}

/// System clock in Unix milliseconds (the default driver clock).
fn system_now_ms() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|duration| duration.as_millis() as u64)
        .unwrap_or(0)
}

/// A plugin action awaiting its typed human-confirmation phrase.
#[derive(Clone, Debug, PartialEq)]
pub struct PendingAction {
    /// The invocation to dispatch once confirmed (its `confirmation` is `None`).
    pub invocation: ActionInvocation,
    /// The required phrase.
    pub phrase: String,
    /// The phrase typed so far.
    pub input: String,
}

/// The full-screen TUI state: the read-model snapshot plus focus, selection,
/// interaction mode, and the last control/notice line.
#[derive(Debug, Clone)]
pub struct App {
    /// Latest read model (executions / components / economics / timeline).
    pub snapshot: Snapshot,
    /// The durable (stateless) plugin panel, kept as the overlay baseline. The
    /// live host layer is always re-derived from this on each authoritative tick,
    /// so a live-only plugin that a later panel omits disappears instead of
    /// lingering.
    pub durable_plugins: Vec<PluginComponent>,
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
    /// Selected plugin row (index into `snapshot.plugins`).
    pub plugin_selection: usize,
    /// Selected action row of the selected plugin.
    pub action_selection: usize,
    /// Set while a plugin action awaits its typed confirmation phrase.
    pub pending_action: Option<PendingAction>,
}

impl App {
    /// Build the app from a snapshot.
    pub fn new(snapshot: Snapshot, transports: Transports, interval_ms: u64) -> Self {
        let durable_plugins = snapshot.plugins.clone();
        let mut app = App {
            snapshot,
            durable_plugins,
            transports,
            interval_ms,
            focus: Pane::Executions,
            selection: 0,
            components_offset: 0,
            mode: Mode::Normal,
            status: None,
            quit: false,
            refresh_requested: false,
            plugin_selection: 0,
            action_selection: 0,
            pending_action: None,
        };
        app.clamp_selection();
        app
    }

    /// Rebuild the snapshot from `state_dir`, preserving focus and clamping the
    /// selection to the new row count. The freshly read panel also becomes the
    /// durable overlay baseline.
    pub fn refresh(&mut self, state_dir: &Path) -> Result<(), String> {
        self.snapshot = crate::status::snapshot_with_transports(state_dir, &self.transports)?;
        self.durable_plugins = self.snapshot.plugins.clone();
        self.clamp_selection();
        Ok(())
    }

    /// Replace the **live** plugin layer over the durable baseline (HP.6
    /// reopened addendum).
    ///
    /// The production host's [`run_interactive_with`] tick supplies the current
    /// registry/report-derived plugin components, including the host's **current**
    /// `PluginHealth` observation (a probe result, not a durable report stream,
    /// so the stateless ledger snapshot can only show the last checkpoint's
    /// health). The panel is authoritative and **replaces** any previous live
    /// layer: it is merged over [`Self::durable_plugins`], never over the
    /// previously overlaid snapshot, so a live-only plugin the new panel omits
    /// disappears and a durable-only entry is preserved. An empty panel therefore
    /// clears every live-only plugin. Ordering (by plugin id) and selection
    /// clamping match a refresh.
    pub fn overlay_plugins(&mut self, live: Vec<PluginComponent>) {
        self.snapshot.plugins = merge_plugin_panel(&self.durable_plugins, live);
        self.clamp_selection();
    }

    /// Drop any live layer and restore the durable baseline (no attached host).
    ///
    /// [`run_interactive_with`] calls this when its tick returns `None` — no
    /// host-supplied panel — so the stateless `status`/`watch` behavior is
    /// exactly what a hostless run renders.
    pub fn clear_live_plugins(&mut self) {
        self.snapshot.plugins = self.durable_plugins.clone();
        self.clamp_selection();
    }

    /// Keep the execution, plugin, and action selections inside the current lists.
    fn clamp_selection(&mut self) {
        let len = self.snapshot.executions.len();
        if len == 0 {
            self.selection = 0;
        } else if self.selection >= len {
            self.selection = len - 1;
        }
        self.clamp_plugin_selection();
    }

    /// The number of advertised actions on the selected plugin.
    fn selected_action_count(&self) -> usize {
        self.selected_plugin()
            .map(|plugin| plugin.actions.len())
            .unwrap_or(0)
    }

    /// Keep the plugin and action selections inside the current lists.
    fn clamp_plugin_selection(&mut self) {
        let plugins = self.snapshot.plugins.len();
        self.plugin_selection = if plugins == 0 {
            0
        } else {
            self.plugin_selection.min(plugins - 1)
        };
        let actions = self.selected_action_count();
        self.action_selection = if actions == 0 {
            0
        } else {
            self.action_selection.min(actions - 1)
        };
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

    /// The plugin the cursor currently selects, if any.
    pub fn selected_plugin(&self) -> Option<&PluginComponent> {
        self.snapshot.plugins.get(self.plugin_selection)
    }

    /// The action the cursor currently selects on the selected plugin, if any.
    pub fn selected_action(&self) -> Option<&ActionView> {
        self.selected_plugin()
            .and_then(|plugin| plugin.actions.get(self.action_selection))
    }

    /// Move the plugin selection by `delta`, saturating at the ends and
    /// re-clamping the action selection to the newly selected plugin.
    pub fn move_plugin_selection(&mut self, delta: isize) {
        let plugins = self.snapshot.plugins.len();
        if plugins == 0 {
            self.plugin_selection = 0;
            self.action_selection = 0;
            return;
        }
        let next = self.plugin_selection as isize + delta;
        self.plugin_selection = next.clamp(0, plugins as isize - 1) as usize;
        let actions = self.selected_action_count();
        self.action_selection = if actions == 0 {
            0
        } else {
            self.action_selection.min(actions - 1)
        };
    }

    /// Move the action selection by `delta`, saturating at the ends.
    pub fn move_action_selection(&mut self, delta: isize) {
        let actions = self.selected_action_count();
        if actions == 0 {
            self.action_selection = 0;
            return;
        }
        let next = self.action_selection as isize + delta;
        self.action_selection = next.clamp(0, actions as isize - 1) as usize;
    }

    /// Handle one key event. Destructive actions collect a typed confirmation
    /// before they are applied through `driver`. Plugin actions are refused: no
    /// action service is bound (the interactive loop uses [`Self::handle_key_with`]).
    pub fn handle_key(&mut self, key: KeyEvent, driver: &mut dyn ControlDriver) {
        let mut actions = UnavailableActionDriver;
        self.handle_key_with(key, driver, &mut actions);
    }

    /// Handle one key event with a bound plugin-action driver. Destructive
    /// control actions collect a typed confirmation before they are applied
    /// through `driver`; a selected plugin action is invoked through `actions`
    /// (which routes through the common validated action service — never a
    /// bypass).
    pub fn handle_key_with(
        &mut self,
        key: KeyEvent,
        driver: &mut dyn ControlDriver,
        actions: &mut dyn ActionDriver,
    ) {
        // Ctrl-C always quits (raw mode delivers it as a key, not a signal).
        if key.modifiers.contains(KeyModifiers::CONTROL)
            && matches!(key.code, KeyCode::Char('c') | KeyCode::Char('C'))
        {
            self.quit = true;
            return;
        }

        // A pending plugin action consumes keys like `Mode::Confirm` does.
        if self.pending_action.is_some() {
            self.handle_pending_action_key(key, actions);
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
            KeyCode::Char('n') | KeyCode::Char('N') => {
                let delta = if key.code == KeyCode::Char('N') { -1 } else { 1 };
                self.move_plugin_selection(delta);
            }
            KeyCode::Char(',') => self.move_action_selection(1),
            KeyCode::Char('.') => self.move_action_selection(-1),
            KeyCode::Char('a') => self.begin_action(actions),
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

    /// Begin invoking the selected plugin action. A declared human confirmation
    /// arms a pending prompt (nothing dispatched yet); otherwise the action is
    /// dispatched immediately through `actions`.
    fn begin_action(&mut self, actions: &mut dyn ActionDriver) {
        let Some(action) = self.selected_action() else {
            self.status = Some("no action selected".to_string());
            return;
        };
        let action_id = action.action_id.clone();
        let human_confirmation = action.human_confirmation.clone();
        let Some(plugin) = self.selected_plugin() else {
            self.status = Some("no action selected".to_string());
            return;
        };
        let Some(generation) = plugin.generation else {
            // Without a pinned generation there is no invocable action.
            self.status = Some("no action selected".to_string());
            return;
        };
        let invocation = ActionInvocation {
            plugin_id: plugin.plugin_id.clone(),
            generation,
            action_id,
            input: serde_json::json!({}),
            confirmation: None,
        };
        match human_confirmation {
            Some(phrase) => {
                self.status = Some(format!("confirm: type \"{phrase}\" then Enter"));
                self.pending_action = Some(PendingAction {
                    invocation,
                    phrase,
                    input: String::new(),
                });
            }
            None => self.dispatch_action(invocation, actions),
        }
    }

    /// Consume a key while a plugin action awaits its typed confirmation phrase
    /// (Esc cancels, Backspace edits, Char appends, Enter validates).
    fn handle_pending_action_key(&mut self, key: KeyEvent, actions: &mut dyn ActionDriver) {
        let Some(pending) = self.pending_action.take() else {
            return;
        };
        let PendingAction {
            invocation,
            phrase,
            mut input,
        } = pending;
        match key.code {
            KeyCode::Esc => {
                self.status = Some("confirmation cancelled".to_string());
            }
            KeyCode::Backspace => {
                input.pop();
                self.pending_action = Some(PendingAction {
                    invocation,
                    phrase,
                    input,
                });
            }
            KeyCode::Enter => {
                if input.trim() == phrase.as_str() {
                    let mut invocation = invocation;
                    invocation.confirmation = Some(phrase);
                    self.dispatch_action(invocation, actions);
                } else {
                    self.status = Some(format!("confirmation mismatch: type \"{phrase}\""));
                    self.pending_action = Some(PendingAction {
                        invocation,
                        phrase,
                        input,
                    });
                }
            }
            KeyCode::Char(c) if !key.modifiers.contains(KeyModifiers::CONTROL) => {
                input.push(c);
                self.pending_action = Some(PendingAction {
                    invocation,
                    phrase,
                    input,
                });
            }
            _ => {
                self.pending_action = Some(PendingAction {
                    invocation,
                    phrase,
                    input,
                });
            }
        }
    }

    /// Dispatch one plugin action through `actions` and reflect the outcome in
    /// the footer. A terminal/delivered outcome refreshes the view; a refusal is
    /// surfaced verbatim.
    fn dispatch_action(&mut self, invocation: ActionInvocation, actions: &mut dyn ActionDriver) {
        let action_id = invocation.action_id.clone();
        match actions.dispatch(&invocation) {
            Ok(outcome) => {
                self.status = Some(format!("{action_id}: {:?}", outcome.state));
                self.refresh_requested = true;
            }
            Err(err) => {
                self.status = Some(format!("{action_id}: error: {err}"));
            }
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
    run_with_actions(args, None).await
}

/// Production entry path with a host-injected plugin-action driver.
///
/// The production host builds the driver over the hub's common [`ActionService`]
/// (catalog + durable ledger journal + pinned-plugin RPC executor + stop
/// admission) with [`service_action_driver`], installing the parent-issued
/// grants, then calls this entry. The interactive loop binds exactly the driver
/// passed here; `None` keeps the `a` binding fail-closed
/// ([`UnavailableActionDriver`]) so the UI never mints action authority. The
/// `--once`/non-TTY path is unaffected (it never dispatches an action).
pub async fn run_with_actions(
    args: &WatchArgs,
    actions: Option<Box<dyn ActionDriver + Send>>,
) -> ExitCode {
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
    match tokio::task::spawn_blocking(move || {
        run_interactive(&state_dir, interval, stop, actions)
    })
    .await
    {
        Ok(code) => code,
        Err(err) => {
            eprintln!("agalma watch: view task failed: {err}");
            ExitCode::FAILURE
        }
    }
}

/// Flip `stop` on SIGINT/SIGTERM so the blocking view can exit cleanly.
///
/// Public so the in-process plugin host can drive the same interactive loop with
/// the same clean-exit semantics.
pub fn spawn_signal_watch(stop: Arc<AtomicBool>) {
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
fn run_interactive(
    state_dir: &Path,
    interval_ms: u64,
    stop: Arc<AtomicBool>,
    actions: Option<Box<dyn ActionDriver + Send>>,
) -> ExitCode {
    run_interactive_with(state_dir, interval_ms, stop, actions, || None)
}

/// The interactive view loop with a host tick callback and an injected
/// plugin-action driver (HP.7 production host attachment; HP.6 live-health
/// overlay).
///
/// `on_tick` runs once per loop iteration before the draw; the in-process plugin
/// host uses it to advance supervision (`supervise_tick`) and refresh its live
/// child-socket map while the operator drives actions through `actions`. It
/// returns the host's current live plugin panel (`PluginHost::plugin_components`)
/// — `Some(components)` when a live host is attached, `None` otherwise. A
/// returned panel is overlaid onto the refreshed snapshot through
/// [`App::overlay_plugins`], so the host's current health observation reaches
/// the shipped UI; `None` restores the durable baseline (no attached host). The driver
/// is bound exactly as [`bind_action_driver`] specifies: the injected production
/// driver when present, else the fail-closed default. Blocking; restores the
/// terminal on all exits.
pub fn run_interactive_with(
    state_dir: &Path,
    interval_ms: u64,
    stop: Arc<AtomicBool>,
    actions: Option<Box<dyn ActionDriver + Send>>,
    mut on_tick: impl FnMut() -> Option<Vec<PluginComponent>>,
) -> ExitCode {
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
    // Bind the host-injected production driver when present (the plugin host
    // attaches it through `run_with_actions`/`run_interactive_with`); otherwise
    // the fail-closed default, so `a` never invents authority.
    let mut actions = bind_action_driver(actions);

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
        // Advance the host (supervision + live sockets) and apply its current
        // live plugin panel before the draw. `Some(panel)` replaces the live
        // layer over the durable baseline (including `Some(empty)`); `None` means
        // no attached host, so the durable baseline is restored.
        match on_tick() {
            Some(live) => app.overlay_plugins(live),
            None => app.clear_live_plugins(),
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
                    app.handle_key_with(key, &mut driver, &mut *actions);
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

/// Merge a live plugin layer over the durable baseline.
///
/// A live component replaces the durable entry with the same plugin id; a
/// durable-only entry is preserved; a live-only entry exists only while the
/// current `live` panel contains it. Ordering is by plugin id (both inputs are
/// already sorted, and a `BTreeMap` keeps the merged order deterministic).
fn merge_plugin_panel(
    durable: &[PluginComponent],
    live: Vec<PluginComponent>,
) -> Vec<PluginComponent> {
    let mut merged: BTreeMap<String, PluginComponent> = durable
        .iter()
        .cloned()
        .map(|component| (component.plugin_id.clone(), component))
        .collect();
    for component in live {
        merged.insert(component.plugin_id.clone(), component);
    }
    merged.into_values().collect()
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
    for (index, component) in app.snapshot.plugins.iter().enumerate() {
        let selected = index == app.plugin_selection;
        let selected_action = if selected {
            Some(app.action_selection)
        } else {
            None
        };
        push_plugin_component(&mut lines, component, selected, selected_action);
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
///
/// The selected plugin row is prefixed `> ` and its selected action is marked in
/// the action list; non-selected rows keep HP.6's read-only rendering.
fn push_plugin_component(
    lines: &mut Vec<Line<'static>>,
    component: &PluginComponent,
    selected: bool,
    selected_action: Option<usize>,
) {
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
    let marker = if selected { "> " } else { "" };
    let plugin_style = if selected {
        Style::new().fg(Color::Cyan).add_modifier(Modifier::BOLD)
    } else {
        Style::new().fg(Color::Cyan)
    };
    let mut spans = vec![Span::styled(
        marker.to_string(),
        Style::new().fg(Color::Yellow).add_modifier(Modifier::BOLD),
    )];
    spans.extend([
        Span::styled(format!("{:<18}", component.plugin_id), plugin_style),
        Span::raw(format!("{:<10} ", component.origin)),
        Span::raw(format!("gen{generation} {:<10} ", component.lifecycle)),
        Span::styled(component.freshness.clone(), freshness_style),
    ]);
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
        let mut spans = vec![Span::styled(
            "    actions: ".to_string(),
            Style::new().fg(Color::Gray),
        )];
        for (index, action) in component.actions.iter().enumerate() {
            if index > 0 {
                spans.push(Span::styled(
                    ", ".to_string(),
                    Style::new().fg(Color::Gray),
                ));
            }
            let is_selected = selected_action == Some(index);
            let text = if is_selected {
                format!("> {}", action.action_id)
            } else {
                action.action_id.clone()
            };
            let style = if is_selected {
                Style::new().fg(Color::Yellow).add_modifier(Modifier::BOLD)
            } else {
                Style::new().fg(Color::Gray)
            };
            spans.push(Span::styled(text, style));
        }
        lines.push(Line::from(spans));
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
    let keys = "q quit   r refresh   Tab/1-4 focus   j/k select   n/N plugin   ,/. action   a invoke   [ ] scroll   p pause   R resume   c cancel";
    let mut lines = vec![Line::from(Span::styled(keys, Style::new().fg(Color::Gray)))];

    if let Some(pending) = &app.pending_action {
        lines.push(Line::from(vec![
            Span::styled(
                format!(
                    "confirm {}: type \"{}\" then Enter (Esc cancels) > ",
                    pending.invocation.action_id, pending.phrase
                ),
                Style::new().fg(Color::Yellow).add_modifier(Modifier::BOLD),
            ),
            Span::raw(pending.input.clone()),
        ]));
    } else {
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

    /// Records every plugin-action invocation and returns a fixed refusal. The
    /// selection/dispatch semantics are what these unit tests exercise; the real
    /// service outcome path is covered by `tests/hp_actions.rs`.
    #[derive(Default)]
    struct RecordingActionDriver {
        calls: Vec<ActionInvocation>,
    }

    impl ActionDriver for RecordingActionDriver {
        fn dispatch(
            &mut self,
            invocation: &ActionInvocation,
        ) -> Result<ActionOutcome, ContractError> {
            self.calls.push(invocation.clone());
            Err(ContractError::UnsupportedCapability(
                "recording action driver".to_string(),
            ))
        }
    }

    /// One advertised action view.
    fn action_view(action_id: &str, confirmation: Option<&str>) -> ActionView {
        ActionView {
            action_id: action_id.to_string(),
            required_capability: "example.control".to_string(),
            effectful: true,
            human_confirmation: confirmation.map(str::to_string),
        }
    }

    /// One plugin component carrying the given advertised actions.
    fn plugin_component(
        plugin_id: &str,
        generation: Option<u64>,
        actions: Vec<ActionView>,
    ) -> PluginComponent {
        PluginComponent {
            plugin_id: plugin_id.to_string(),
            origin: "discovered".to_string(),
            desired: "running".to_string(),
            lifecycle: "ready".to_string(),
            generation,
            freshness: "fresh".to_string(),
            stale: false,
            disconnected: false,
            incomplete: false,
            last_error: None,
            health: None,
            actions,
            views: Vec::new(),
        }
    }

    /// Two plugins: `example.a` with a direct and a confirmation-required action,
    /// and `example.b` with one direct action.
    fn plugins_snapshot() -> Snapshot {
        let mut snapshot = present();
        snapshot.plugins = vec![
            plugin_component(
                "example.a",
                Some(1),
                vec![
                    action_view("a.refresh", None),
                    action_view("a.reset", Some("confirm-reset")),
                ],
            ),
            plugin_component("example.b", Some(2), vec![action_view("b.sync", None)]),
        ];
        snapshot
    }

    #[test]
    fn action_key_dispatches_selected_non_confirmation_action() {
        let mut app = app(plugins_snapshot());
        let mut driver = RecordingDriver::applied();
        let mut actions = RecordingActionDriver::default();

        assert_eq!(app.selected_plugin().map(|p| p.plugin_id.as_str()), Some("example.a"));
        app.handle_key_with(press(KeyCode::Char('a')), &mut driver, &mut actions);

        assert_eq!(actions.calls.len(), 1, "exactly one dispatch");
        let call = &actions.calls[0];
        assert_eq!(call.plugin_id, "example.a");
        assert_eq!(call.generation, 1);
        assert_eq!(call.action_id, "a.refresh");
        assert!(call.confirmation.is_none(), "no confirmation required");
        assert!(app.pending_action.is_none());
    }

    #[test]
    fn confirmation_action_arms_then_dispatches_on_exact_phrase() {
        let mut app = app(plugins_snapshot());
        let mut driver = RecordingDriver::applied();
        let mut actions = RecordingActionDriver::default();

        // Move to the confirmation-required action (index 1).
        app.handle_key_with(press(KeyCode::Char(',')), &mut driver, &mut actions);
        assert_eq!(app.action_selection, 1);

        app.handle_key_with(press(KeyCode::Char('a')), &mut driver, &mut actions);
        assert!(actions.calls.is_empty(), "nothing dispatched before confirmation");
        assert!(app.pending_action.is_some());
        assert!(app
            .status
            .as_deref()
            .unwrap_or("")
            .contains("confirm: type \"confirm-reset\""));

        // Wrong phrase + Enter dispatches nothing and keeps the prompt.
        for c in "wrong".chars() {
            app.handle_key_with(press(KeyCode::Char(c)), &mut driver, &mut actions);
        }
        app.handle_key_with(press(KeyCode::Enter), &mut driver, &mut actions);
        assert!(actions.calls.is_empty(), "a mismatch dispatches nothing");
        assert!(app.pending_action.is_some(), "the prompt stays");
        assert!(app
            .status
            .as_deref()
            .unwrap_or("")
            .contains("confirmation mismatch"));

        // Clear and type the exact phrase; Enter dispatches once with it.
        for _ in 0.."wrong".len() {
            app.handle_key_with(press(KeyCode::Backspace), &mut driver, &mut actions);
        }
        for c in "confirm-reset".chars() {
            app.handle_key_with(press(KeyCode::Char(c)), &mut driver, &mut actions);
        }
        app.handle_key_with(press(KeyCode::Enter), &mut driver, &mut actions);
        assert_eq!(actions.calls.len(), 1, "the confirmed action dispatches once");
        assert_eq!(actions.calls[0].action_id, "a.reset");
        assert_eq!(
            actions.calls[0].confirmation.as_deref(),
            Some("confirm-reset")
        );
        assert!(app.pending_action.is_none());
    }

    #[test]
    fn pending_action_esc_cancels_without_dispatch() {
        let mut app = app(plugins_snapshot());
        let mut driver = RecordingDriver::applied();
        let mut actions = RecordingActionDriver::default();

        app.handle_key_with(press(KeyCode::Char(',')), &mut driver, &mut actions);
        app.handle_key_with(press(KeyCode::Char('a')), &mut driver, &mut actions);
        assert!(app.pending_action.is_some());

        app.handle_key_with(press(KeyCode::Esc), &mut driver, &mut actions);
        assert!(app.pending_action.is_none());
        assert!(actions.calls.is_empty());
        assert!(app
            .status
            .as_deref()
            .unwrap_or("")
            .contains("confirmation cancelled"));
    }

    #[test]
    fn n_and_comma_move_plugin_and_action_selection() {
        let mut app = app(plugins_snapshot());
        let mut driver = RecordingDriver::applied();
        let mut actions = RecordingActionDriver::default();

        assert_eq!(app.plugin_selection, 0);
        assert_eq!(app.action_selection, 0);

        app.handle_key_with(press(KeyCode::Char('n')), &mut driver, &mut actions);
        assert_eq!(app.plugin_selection, 1);
        app.handle_key_with(press(KeyCode::Char('N')), &mut driver, &mut actions);
        assert_eq!(app.plugin_selection, 0);
        // Saturates at the first plugin.
        app.handle_key_with(press(KeyCode::Char('N')), &mut driver, &mut actions);
        assert_eq!(app.plugin_selection, 0);

        // Plugin 0 has two actions: ',' advances, '.' retreats, both saturate.
        app.handle_key_with(press(KeyCode::Char(',')), &mut driver, &mut actions);
        assert_eq!(app.action_selection, 1);
        app.handle_key_with(press(KeyCode::Char(',')), &mut driver, &mut actions);
        assert_eq!(app.action_selection, 1, "saturates at last action");
        app.handle_key_with(press(KeyCode::Char('.')), &mut driver, &mut actions);
        assert_eq!(app.action_selection, 0);
        app.handle_key_with(press(KeyCode::Char('.')), &mut driver, &mut actions);
        assert_eq!(app.action_selection, 0, "saturates at first action");
    }

    #[test]
    fn action_selection_reclamps_when_plugin_changes() {
        let mut app = app(plugins_snapshot());
        let mut driver = RecordingDriver::applied();
        let mut actions = RecordingActionDriver::default();

        // Move to plugin 0's second action, then to plugin 1 (only one action).
        app.handle_key_with(press(KeyCode::Char(',')), &mut driver, &mut actions);
        assert_eq!(app.action_selection, 1);
        app.handle_key_with(press(KeyCode::Char('n')), &mut driver, &mut actions);
        assert_eq!(app.plugin_selection, 1);
        assert_eq!(app.action_selection, 0, "re-clamped to plugin 1's single action");
        assert_eq!(app.selected_action().map(|a| a.action_id.as_str()), Some("b.sync"));
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
