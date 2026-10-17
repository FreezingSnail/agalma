//! Agalma conductor.
//!
//! Composition root for the Agalma factory (see [`composition`]); M0.8 wires the
//! real phase activities with recovery reconciliation (see [`phases`]) so
//! `agalma run --fixture <dir> --once` executes one hardcoded task end-to-end and
//! a restart resumes the durable execution at its phase. Dependency direction is
//! `conductor -> implementations -> contracts`; the architecture lint in
//! `tests/architecture.rs` enforces impl-to-impl isolation.

pub mod budget;
pub mod cli;
pub mod components;
pub mod composition;
pub mod concierge;
pub mod config;
pub mod control;
pub mod designer;
pub mod digest;
pub mod economics;
pub mod execution_process;
pub mod harness_binding;
pub mod harness_process;
pub mod ledger_process;
pub mod phases;
pub mod plan_check;
pub mod plugin_manifest;
pub mod plugin_registry;
pub mod plugin_supervisor;
pub mod provider_auth;
pub mod provider_proxy;
pub mod reap;
pub mod reconcile;
pub mod review;
pub mod routing;
pub mod scope_guard;
pub mod session;
pub mod status;
pub mod stop;
pub mod sweep;
pub mod task;
pub mod task_package;
pub mod taskqueue_process;
pub mod watch;
pub mod work;
pub mod workers;
pub mod workspace_process;

/// Canonical concierge role prompt (CP.2, `agalma-i5h.3`): goal → epic + design.
pub const CONCIERGE_PROMPT: &str = include_str!("../prompts/concierge.md");
/// Canonical designer role prompt (CP.2, `agalma-i5h.3`): epic design → staged
/// decomposition.
pub const DESIGNER_PROMPT: &str = include_str!("../prompts/designer.md");
/// Canonical reviewer role prompt (CP.5, `agalma-i5h.6`): merged diff vs epic
/// design/acceptance → exactly one `REVIEW:` marker.
pub const REVIEWER_PROMPT: &str = include_str!("../prompts/reviewer.md");

use std::fs::OpenOptions;
use std::io::Write;
use std::path::PathBuf;
use std::process::ExitCode;

use agalma_contracts::{
    ExecutionApi, ExecutionId, ExecutionPhase, ExecutionState, ExecutionStatus, LedgerApi,
    StepOutcome, TaskId,
};
use agalma_execution::{
    Executor, ExecutorConfig, EXECUTION_DEFINITION_VERSION, LEDGER_SCHEMA_VERSION,
};
use agalma_ipc::LedgerClient;
use agalma_ledger::SqliteLedger;
use clap::Parser;
use tokio::signal::unix::{signal, SignalKind};

use crate::cli::{
    CancelArgs, Cli, Command, DigestArgs, GcArgs, ReconcileArgs, RunArgs, StateArgs, StatusArgs,
    WatchArgs,
};
use crate::composition::Composition;
use crate::config::{Config, PHASE_PLAN};
use crate::ledger_process::{ledger_socket, LedgerHandle, LedgerTransport};
use crate::taskqueue_process::{TaskQueueHandle, TaskQueueTransport};

/// Parse the CLI and run the conductor.
pub async fn run() -> ExitCode {
    let cli = Cli::parse();
    match cli.command {
        Command::Run(args) => run_command(args).await,
        Command::Work(args) => crate::work::run_work(args),
        Command::Reconcile(args) => reconcile_command(&args),
        Command::Resume(args) => resume_command(&args),
        Command::Pause(args) => pause_command(&args),
        Command::Cancel(args) => cancel_command(&args),
        Command::Status(args) => status_command(&args),
        Command::Watch(args) => watch_command(&args).await,
        Command::Digest(args) => digest_command(&args),
        Command::Models => models_command(),
        Command::PlanCheck(args) => crate::plan_check::run_plan_check(&args),
        Command::Concierge(args) => crate::concierge::run_concierge_command(&args),
        Command::Designer(args) => crate::designer::run_designer_command(&args),
        Command::Gc(args) => gc_command(&args),
        Command::Plugins(args) => crate::composition::run_plugin_host(&args).await,
    }
}

/// `agalma models`: print the H3 model table, active routes, and allow-paid
/// state (no state dir needed; the policy is static). The paid-credential state
/// gates the paid-only planner route.
fn models_command() -> ExitCode {
    let allow_paid = crate::routing::allow_paid_from_env();
    let paid_available = crate::routing::paid_credential_available();
    print!(
        "{}",
        crate::routing::render_models(allow_paid, paid_available)
    );
    ExitCode::SUCCESS
}

async fn run_command(args: RunArgs) -> ExitCode {
    let config = match Config::resolve(&args) {
        Ok(config) => config,
        Err(err) => {
            eprintln!("agalma: {err}");
            return ExitCode::FAILURE;
        }
    };

    print_header(&config);

    let _lock = match LockGuard::acquire(&config.state_dir) {
        Ok(lock) => lock,
        Err(err) => {
            eprintln!("agalma: {err}");
            return ExitCode::FAILURE;
        }
    };

    let mut composition = match Composition::build(&config) {
        Ok(composition) => composition,
        Err(err) => {
            eprintln!("agalma: composition root failed: {err}");
            return ExitCode::FAILURE;
        }
    };

    // Boot gate 1: refuse to run under an incompatible physical schema. The
    // executor also parks dispatch under a mismatch, but running is pointless.
    match composition.executor.ledger().schema_version() {
        Ok(version) if version == LEDGER_SCHEMA_VERSION => {}
        Ok(version) => {
            eprintln!(
                "agalma: incompatible ledger schema {version} (expected {LEDGER_SCHEMA_VERSION}) \
                 at {}; refusing to run",
                composition.ledger_path().display()
            );
            return ExitCode::FAILURE;
        }
        Err(err) => {
            eprintln!("agalma: cannot read ledger schema: {err}");
            return ExitCode::FAILURE;
        }
    }

    install_signal_handler(composition.state_dir.clone(), LedgerTransport::from_env());

    let task_id = TaskId::new(composition.task.id.clone());
    // Boot gates 2-4 (replay, projection agreement, survivor reconciliation) ran
    // inside `Composition::build` via `Executor::recover`. Resume the durable
    // execution when one is incomplete, else start a fresh generation.
    let execution = match resume_or_start(&mut composition.executor, &task_id) {
        Ok(execution) => execution,
        Err(err) => {
            eprintln!("agalma: cannot start execution: {err}");
            return ExitCode::FAILURE;
        }
    };
    println!("execution: {execution}");

    let state_dir = composition.state_dir.clone();
    let status = match drive(&mut composition.executor, &execution, &*composition.stop) {
        Ok(status) => status,
        Err(err) => {
            eprintln!("agalma: run failed: {err}");
            return ExitCode::FAILURE;
        }
    };

    report(&status);
    if crate::reap::is_terminal(status.phase, status.state) {
        let reap = crate::reap::reap_execution(&state_dir, &execution);
        eprintln!("agalma: reap {}", crate::reap::summary_line(&reap));
    }
    if status.phase == ExecutionPhase::Done {
        ExitCode::SUCCESS
    } else {
        ExitCode::FAILURE
    }
}

/// Resume an incomplete execution for `task_id`, or start a new generation.
///
/// A completed, parked, or failed execution is terminal: a restart starts a new
/// generation instead of resurrecting it.
fn resume_or_start(
    executor: &mut Executor<LedgerHandle>,
    task_id: &TaskId,
) -> Result<ExecutionId, agalma_contracts::ContractError> {
    let mut candidate: Option<(u32, ExecutionId)> = None;
    for record in executor.ledger().executions()? {
        if &record.task_id != task_id {
            continue;
        }
        let terminal = matches!(
            record.state,
            ExecutionState::Completed | ExecutionState::Parked | ExecutionState::Failed
        ) || matches!(record.phase, ExecutionPhase::Done | ExecutionPhase::Parked);
        if terminal {
            continue;
        }
        if candidate
            .as_ref()
            .map(|(generation, _)| record.generation > *generation)
            .unwrap_or(true)
        {
            candidate = Some((record.generation, record.execution_id.clone()));
        }
    }
    match candidate {
        Some((_, execution)) => Ok(execution),
        None => executor.start(task_id),
    }
}

/// Drive one execution to a terminal state (or a stop signal).
fn drive(
    executor: &mut Executor<LedgerHandle>,
    execution: &ExecutionId,
    stop: &dyn crate::stop::StopControl,
) -> Result<ExecutionStatus, agalma_contracts::ContractError> {
    // The shared bound stop decision is checked before every dispatch: a
    // sentinel/latch sets the kill latch and halts; an unreadable established
    // ledger (Unavailable) halts without mutating state (`agalma-5fy.5`).
    loop {
        match stop.decision() {
            crate::stop::StopDecision::Running => {}
            crate::stop::StopDecision::Stopped(reason) => {
                executor.set_kill_latch(true)?;
                eprintln!("agalma: stop policy active ({reason}); kill latch set");
                break;
            }
            crate::stop::StopDecision::Unavailable(reason) => {
                eprintln!("agalma: stop state unavailable ({reason}); halting");
                break;
            }
        }
        match executor.step(execution)? {
            StepOutcome::Dispatched { .. } | StepOutcome::Advanced { .. } => continue,
            StepOutcome::Idle { .. } => break,
            StepOutcome::Parked { reason } => {
                eprintln!("agalma: parked: {reason}");
                break;
            }
            StepOutcome::Blocked { .. } => {
                eprintln!("agalma: dispatch blocked (kill latch set)");
                break;
            }
        }
    }
    executor.status(execution)
}

/// SIGTERM/SIGINT: persist the kill latch through the bound ledger (a second
/// connection for the in-process binding, a fresh client for the process
/// binding) and exit cleanly. Task owner: M0.8. Survivor workers left behind are
/// reconciled on the next boot.
pub(crate) fn install_signal_handler(state_dir: PathBuf, transport: LedgerTransport) {
    tokio::spawn(async move {
        let mut sigint = match signal(SignalKind::interrupt()) {
            Ok(stream) => stream,
            Err(err) => {
                eprintln!("agalma: cannot install SIGINT handler: {err}");
                return;
            }
        };
        let mut sigterm = match signal(SignalKind::terminate()) {
            Ok(stream) => stream,
            Err(err) => {
                eprintln!("agalma: cannot install SIGTERM handler: {err}");
                return;
            }
        };
        tokio::select! {
            _ = sigint.recv() => {}
            _ = sigterm.recv() => {}
        }
        let result = match transport {
            LedgerTransport::InProcess => SqliteLedger::open(state_dir.join("ledger.sqlite"))
                .and_then(|mut ledger| ledger.set_kill_latch(true)),
            LedgerTransport::Process => {
                LedgerClient::connect(&ledger_socket(&state_dir), &state_dir)
                    .map_err(|e| agalma_contracts::ContractError::KnownFailure(e.to_string()))
                    .and_then(|mut client| client.set_kill_latch(true))
            }
        };
        if let Err(err) = result {
            eprintln!("agalma: cannot persist kill latch: {err}");
        }
        eprintln!("agalma: signal received; kill latch persisted; exiting");
        std::process::exit(0);
    });
}

fn print_header(config: &Config) {
    println!("agalma run");
    println!("fixture:   {}", config.fixture.display());
    println!("state_dir: {}", config.state_dir.display());
    println!("model:     {}", config.model);
    println!("phases:    {}", PHASE_PLAN.join(" -> "));
}

fn report(status: &ExecutionStatus) {
    println!(
        "result: phase={:?} state={:?} attempt={} parked_reason={:?}",
        status.phase, status.state, status.attempt, status.parked_reason
    );
}

fn resume_command(args: &StateArgs) -> ExitCode {
    latch_command(args, crate::control::ControlAction::Resume)
}

/// `agalma pause`: set the durable kill latch (stops new claims/dispatch;
/// in-flight work is not interrupted). Human-only; records a receipt.
fn pause_command(args: &StateArgs) -> ExitCode {
    latch_command(args, crate::control::ControlAction::Pause)
}

/// Pause/Resume: open the ledger, apply through the control plane (guard +
/// durable receipt), and report the outcome.
///
/// The subcommand invocation is itself the human's typed confirmation for
/// resume; pause is non-destructive and needs none.
fn latch_command(args: &StateArgs, action: crate::control::ControlAction) -> ExitCode {
    let state_dir = match crate::config::resolve_state_dir(args.state_dir.as_deref()) {
        Ok(dir) => dir,
        Err(err) => {
            eprintln!("agalma: {err}");
            return ExitCode::FAILURE;
        }
    };
    let ledger_path = state_dir.join("ledger.sqlite");
    let mut ledger = match SqliteLedger::open(&ledger_path) {
        Ok(ledger) => ledger,
        Err(err) => {
            eprintln!(
                "agalma: cannot open ledger {}: {err}",
                ledger_path.display()
            );
            return ExitCode::FAILURE;
        }
    };
    let mut cancel = crate::control::NoCancel;
    let mut request = human_request(action);
    if request.action.requires_confirmation() {
        let word = request.action.name();
        request = request.with_confirmation(word);
    }
    let outcome = {
        let mut plane = crate::control::ControlPlane::new(&mut ledger, &mut cancel, &state_dir);
        match plane.apply(&request) {
            Ok(outcome) => outcome,
            Err(err) => {
                eprintln!("agalma {}: {err}", request.action.name());
                return ExitCode::FAILURE;
            }
        }
    };
    report_control(&request, &outcome)
}

/// `agalma cancel --execution <id> [--confirm cancel]`: request cancellation of
/// one execution (human-only, typed confirmation). Records a durable receipt
/// and, on success, the `cancel_requested` event via [`ExecutionApi::cancel`].
fn cancel_command(args: &CancelArgs) -> ExitCode {
    let state_dir = match crate::config::resolve_state_dir(args.state_dir.as_deref()) {
        Ok(dir) => dir,
        Err(err) => {
            eprintln!("agalma: {err}");
            return ExitCode::FAILURE;
        }
    };
    let ledger_path = state_dir.join("ledger.sqlite");
    if !ledger_path.exists() {
        eprintln!("agalma: no ledger at {}", ledger_path.display());
        return ExitCode::FAILURE;
    }
    // The control plane records receipts through its own connection; the
    // executor (the cancel seam) holds a second, so both borrow `&mut` cleanly.
    let mut plane_ledger = match SqliteLedger::open(&ledger_path) {
        Ok(ledger) => ledger,
        Err(err) => {
            eprintln!("agalma: cannot open ledger: {err}");
            return ExitCode::FAILURE;
        }
    };
    let exec_ledger = match SqliteLedger::open(&ledger_path) {
        Ok(ledger) => ledger,
        Err(err) => {
            eprintln!("agalma: cannot open ledger: {err}");
            return ExitCode::FAILURE;
        }
    };
    let mut executor = Executor::new(
        exec_ledger,
        ExecutorConfig {
            max_attempts: 1,
            execution_definition_version: EXECUTION_DEFINITION_VERSION,
        },
    );
    let action = crate::control::ControlAction::Cancel {
        execution: ExecutionId::new(args.execution.clone()),
    };
    let mut request = human_request(action);
    if let Some(word) = &args.confirm {
        request = request.with_confirmation(word.clone());
    }
    let outcome = {
        let mut plane =
            crate::control::ControlPlane::new(&mut plane_ledger, &mut executor, &state_dir);
        match plane.apply(&request) {
            Ok(outcome) => outcome,
            Err(err) => {
                eprintln!("agalma cancel: {err}");
                return ExitCode::FAILURE;
            }
        }
    };
    report_control(&request, &outcome)
}

/// Build a human control request with a fresh action id.
fn human_request(action: crate::control::ControlAction) -> crate::control::ControlRequest {
    crate::control::ControlRequest::new(
        crate::control::new_action_id(&action),
        action,
        crate::control::Actor::Human,
    )
}

/// Print the control outcome; a denial is a failure exit (nothing changed).
fn report_control(
    request: &crate::control::ControlRequest,
    outcome: &crate::control::ControlOutcome,
) -> ExitCode {
    match outcome {
        crate::control::ControlOutcome::Applied => {
            println!(
                "agalma {}: applied (receipt {})",
                request.action.name(),
                request.action_id
            );
            ExitCode::SUCCESS
        }
        crate::control::ControlOutcome::Denied { reason } => {
            eprintln!("agalma {}: denied: {reason}", request.action.name());
            ExitCode::FAILURE
        }
    }
}

/// `agalma reconcile`: compare the ledger against the bd projection and rewrite
/// the projection to match (ledger wins). Prints a reconcile summary.
fn reconcile_command(args: &ReconcileArgs) -> ExitCode {
    let state_dir = match crate::config::resolve_state_dir(args.state_dir.as_deref()) {
        Ok(dir) => dir,
        Err(err) => {
            eprintln!("agalma: {err}");
            return ExitCode::FAILURE;
        }
    };
    let repo = match std::fs::canonicalize(&args.repo) {
        Ok(repo) => repo,
        Err(err) => {
            eprintln!("agalma: repo {}: {err}", args.repo.display());
            return ExitCode::FAILURE;
        }
    };
    let ledger_path = state_dir.join("ledger.sqlite");
    if !ledger_path.exists() {
        println!("agalma: no ledger at {}", ledger_path.display());
        return ExitCode::SUCCESS;
    }
    let ledger = match SqliteLedger::open(&ledger_path) {
        Ok(ledger) => ledger,
        Err(err) => {
            eprintln!(
                "agalma: cannot open ledger {}: {err}",
                ledger_path.display()
            );
            return ExitCode::FAILURE;
        }
    };
    let mut queue =
        match TaskQueueHandle::from_transport(&state_dir, &repo, TaskQueueTransport::from_env()) {
            Ok(queue) => queue,
            Err(err) => {
                eprintln!("agalma: cannot bind task queue: {err}");
                return ExitCode::FAILURE;
            }
        };
    match crate::reconcile::reconcile_projection(&ledger, &mut queue) {
        Ok(summary) => {
            println!("{}", summary.render());
            ExitCode::SUCCESS
        }
        Err(err) => {
            eprintln!("agalma: reconcile failed: {err}");
            ExitCode::FAILURE
        }
    }
}

/// `agalma gc`: reap the heavy scratch of every terminal execution.
///
/// The manual counterpart to the boot sweep `agalma work` runs on every start:
/// needed for older or hand-managed state dirs. Resolves the state dir, refuses
/// while a conductor holds the state-dir lock, opens the ledger through the same
/// transport and schema gate as `work`, then deletes each terminal execution's
/// heavyweight temp trees ([`crate::reap::reap_terminal`]) while keeping the
/// small forensic evidence. Prints one reap line per execution (the module) plus
/// a total; `nothing to reap` when no execution had scratch left. Exit 0.
pub fn gc_command(args: &GcArgs) -> ExitCode {
    let state_dir = match crate::config::resolve_state_dir(args.state_dir.as_deref()) {
        Ok(dir) => dir,
        Err(err) => {
            eprintln!("agalma: {err}");
            return ExitCode::FAILURE;
        }
    };
    // Canonicalize when the dir already exists; `LockGuard` creates it otherwise.
    let state_dir = std::fs::canonicalize(&state_dir).unwrap_or(state_dir);

    let _lock = match LockGuard::acquire(&state_dir) {
        Ok(lock) => lock,
        Err(err) => {
            eprintln!("agalma: {err}");
            return ExitCode::FAILURE;
        }
    };

    let ledger = match LedgerHandle::from_transport(&state_dir, LedgerTransport::from_env()) {
        Ok(ledger) => ledger,
        Err(err) => {
            eprintln!("agalma: cannot open ledger: {err}");
            return ExitCode::FAILURE;
        }
    };
    // Same schema gate as `work`: refuse under an incompatible physical schema.
    match ledger.schema_version() {
        Ok(version) if version == LEDGER_SCHEMA_VERSION => {}
        Ok(version) => {
            eprintln!(
                "agalma: incompatible ledger schema {version} (expected {LEDGER_SCHEMA_VERSION}) \
                 at {}; refusing to run",
                state_dir.join("ledger.sqlite").display()
            );
            return ExitCode::FAILURE;
        }
        Err(err) => {
            eprintln!("agalma: cannot read ledger schema: {err}");
            return ExitCode::FAILURE;
        }
    }

    let reports = crate::reap::reap_terminal(&state_dir, &ledger);
    let reaped = reports.iter().filter(|r| !r.removed.is_empty()).count();
    if reaped == 0 {
        println!("agalma gc: nothing to reap");
    } else {
        let bytes: u64 = reports.iter().map(|r| r.bytes).sum();
        let mb = bytes as f64 / (1024.0 * 1024.0);
        println!("agalma gc: reaped {reaped} executions, {mb:.2} MB");
    }
    ExitCode::SUCCESS
}

/// `agalma status [--json]`: report ledger and execution status.
///
/// Without `--json`, prints the human-oriented report. With `--json`, emits the
/// TUI read-model snapshot ([`crate::status`]): a single JSON document carrying
/// the state-dir presence (`exists`), the kill latch, the ledger schema version,
/// the current harness binding, and one entry per execution. Both renderings
/// come from the one snapshot, so the derivation lives in exactly one place.
fn status_command(args: &StatusArgs) -> ExitCode {
    let state_dir = match crate::config::resolve_state_dir(args.state_dir.as_deref()) {
        Ok(dir) => dir,
        Err(err) => {
            eprintln!("agalma: {err}");
            return ExitCode::FAILURE;
        }
    };
    let snapshot = match crate::status::snapshot(&state_dir) {
        Ok(snapshot) => snapshot,
        Err(err) => {
            eprintln!("agalma: {err}");
            return ExitCode::FAILURE;
        }
    };
    if args.json {
        return match crate::status::to_json(&snapshot) {
            Ok(text) => {
                println!("{text}");
                ExitCode::SUCCESS
            }
            Err(err) => {
                eprintln!("agalma: {err}");
                ExitCode::FAILURE
            }
        };
    }
    if !snapshot.exists {
        println!(
            "agalma: no ledger at {}",
            state_dir.join("ledger.sqlite").display()
        );
        return ExitCode::SUCCESS;
    }
    if snapshot.executions.is_empty() {
        println!("no executions");
    }
    for row in &snapshot.executions {
        println!(
            "{} phase={} state={} attempt={} parked_reason={:?} harness={}",
            row.execution_id, row.phase, row.state, row.attempt, row.parked_reason, row.harness
        );
    }
    println!("kill_latch={}", snapshot.kill_latch);
    ExitCode::SUCCESS
}

/// `agalma watch`: live refresh loop over the TUI read model (read-only).
///
/// Delegates to [`crate::watch::run`]: `--once` (or a non-TTY stdout) renders a
/// single frame; otherwise an alternate-screen loop refreshes until `q` +
/// Enter, Ctrl-C/SIGTERM, or stdin EOF. Never writes.
async fn watch_command(args: &WatchArgs) -> ExitCode {
    crate::watch::run(args).await
}

/// `agalma digest --execution <id>`: print the latest recorded digest (JSON,
/// then the one-line summary).
fn digest_command(args: &DigestArgs) -> ExitCode {
    let state_dir = match crate::config::resolve_state_dir(args.state_dir.as_deref()) {
        Ok(dir) => dir,
        Err(err) => {
            eprintln!("agalma: {err}");
            return ExitCode::FAILURE;
        }
    };
    let ledger_path = state_dir.join("ledger.sqlite");
    if !ledger_path.exists() {
        eprintln!("agalma: no ledger at {}", ledger_path.display());
        return ExitCode::FAILURE;
    }
    let ledger = match SqliteLedger::open(&ledger_path) {
        Ok(ledger) => ledger,
        Err(err) => {
            eprintln!(
                "agalma: cannot open ledger {}: {err}",
                ledger_path.display()
            );
            return ExitCode::FAILURE;
        }
    };
    let execution = ExecutionId::new(args.execution.clone());
    let digests = match ledger.digests_for_execution(&execution) {
        Ok(digests) => digests,
        Err(err) => {
            eprintln!("agalma: cannot read digests: {err}");
            return ExitCode::FAILURE;
        }
    };
    let Some(latest) = digests.last() else {
        eprintln!("agalma: no digest recorded for {execution}");
        return ExitCode::FAILURE;
    };
    match serde_json::to_string_pretty(&latest.summary) {
        Ok(text) => println!("{text}"),
        Err(_) => println!("{}", latest.summary),
    }
    if let Ok(digest) = serde_json::from_value::<crate::digest::Digest>(latest.summary.clone()) {
        println!("{}", digest.summary_line());
    }
    ExitCode::SUCCESS
}

/// Best-effort exclusive lock file in the state dir.
///
/// A live pid holding the lock refuses a second conductor; a stale lock (dead
/// pid) is reclaimed. Removed on drop. Public so integration tests can hold the
/// lock to exercise the refusal path (`agalma gc`, `agalma work`).
pub struct LockGuard {
    path: PathBuf,
}

impl LockGuard {
    pub fn acquire(state_dir: &std::path::Path) -> Result<Self, String> {
        std::fs::create_dir_all(state_dir)
            .map_err(|e| format!("cannot create state dir {}: {e}", state_dir.display()))?;
        let path = state_dir.join("conductor.lock");
        for attempt in 0..2 {
            match OpenOptions::new().write(true).create_new(true).open(&path) {
                Ok(mut file) => {
                    let _ = writeln!(file, "{}", std::process::id());
                    return Ok(LockGuard { path });
                }
                Err(err) if err.kind() == std::io::ErrorKind::AlreadyExists => {
                    if attempt == 0 && !lock_holder_alive(&path) {
                        let _ = std::fs::remove_file(&path);
                        continue;
                    }
                    return Err(format!(
                        "another conductor appears to be running (lock {})",
                        path.display()
                    ));
                }
                Err(err) => return Err(format!("cannot create lock {}: {err}", path.display())),
            }
        }
        Err(format!("cannot acquire lock {}", path.display()))
    }
}

impl Drop for LockGuard {
    fn drop(&mut self) {
        let _ = std::fs::remove_file(&self.path);
    }
}

fn lock_holder_alive(path: &std::path::Path) -> bool {
    let Ok(text) = std::fs::read_to_string(path) else {
        return false;
    };
    let Ok(pid) = text.trim().parse::<u32>() else {
        return false;
    };
    std::process::Command::new("/bin/kill")
        .arg("-0")
        .arg(pid.to_string())
        .output()
        .map(|out| out.status.success())
        .unwrap_or(false)
}
