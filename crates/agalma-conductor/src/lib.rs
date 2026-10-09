//! Agalma conductor.
//!
//! Composition root for the Agalma factory (see [`composition`]); M0.7 wires the
//! real phase activities (see [`phases`]) so `agalma run --fixture <dir> --once`
//! executes one hardcoded task end-to-end. Dependency direction is
//! `conductor -> implementations -> contracts`; the architecture lint in
//! `tests/architecture.rs` enforces impl-to-impl isolation.

pub mod cli;
pub mod composition;
pub mod config;
pub mod phases;
pub mod provider_proxy;
pub mod task;

use std::fs::OpenOptions;
use std::io::Write;
use std::path::PathBuf;
use std::process::ExitCode;

use agalma_contracts::{
    ExecutionApi, ExecutionPhase, ExecutionStatus, LedgerApi, StepOutcome, TaskId,
};
use agalma_ledger::SqliteLedger;
use clap::Parser;
use tokio::signal::unix::{signal, SignalKind};

use crate::cli::{Cli, Command, RunArgs};
use crate::composition::Composition;
use crate::config::{Config, PHASE_PLAN};

/// Parse the CLI and run the conductor.
pub async fn run() -> ExitCode {
    let cli = Cli::parse();
    match cli.command {
        Command::Run(args) => run_command(args).await,
        Command::Resume => resume_command(),
        Command::Status => status_command(),
    }
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

    install_signal_handler(composition.ledger_path().to_path_buf());

    let task_id = TaskId::new(composition.task.id.clone());
    let execution = match composition.executor.start(&task_id) {
        Ok(execution) => execution,
        Err(err) => {
            eprintln!("agalma: cannot start execution: {err}");
            return ExitCode::FAILURE;
        }
    };
    println!("execution: {execution}");

    let state_dir = composition.state_dir.clone();
    let status = match drive(&mut composition.executor, &execution, &state_dir) {
        Ok(status) => status,
        Err(err) => {
            eprintln!("agalma: run failed: {err}");
            return ExitCode::FAILURE;
        }
    };

    report(&status);
    if status.phase == ExecutionPhase::Done {
        ExitCode::SUCCESS
    } else {
        ExitCode::FAILURE
    }
}

/// Drive one execution to a terminal state (or a stop signal).
fn drive(
    executor: &mut agalma_execution::Executor<SqliteLedger>,
    execution: &agalma_contracts::ExecutionId,
    state_dir: &std::path::Path,
) -> Result<ExecutionStatus, agalma_contracts::ContractError> {
    // Sentinel checked before every dispatch (design: `<state>/STOP`).
    let stop = state_dir.join("STOP");
    loop {
        if stop.exists() {
            executor.set_kill_latch(true)?;
            eprintln!("agalma: STOP sentinel present; kill latch set");
            break;
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

/// SIGTERM/SIGINT: persist the kill latch through a second ledger connection and
/// exit cleanly. Full recovery/termination accounting is M0.8.
fn install_signal_handler(ledger_path: PathBuf) {
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
        match SqliteLedger::open(&ledger_path) {
            Ok(mut ledger) => {
                if let Err(err) = ledger.set_kill_latch(true) {
                    eprintln!("agalma: cannot persist kill latch: {err}");
                }
            }
            Err(err) => eprintln!("agalma: cannot open ledger for latch: {err}"),
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

fn resume_command() -> ExitCode {
    let state_dir = match crate::config::resolve_state_dir(None) {
        Ok(dir) => dir,
        Err(err) => {
            eprintln!("agalma: {err}");
            return ExitCode::FAILURE;
        }
    };
    let ledger_path = state_dir.join("ledger.sqlite");
    match SqliteLedger::open(&ledger_path) {
        Ok(mut ledger) => {
            if let Err(err) = ledger.set_kill_latch(false) {
                eprintln!("agalma: cannot clear kill latch: {err}");
                return ExitCode::FAILURE;
            }
        }
        Err(err) => {
            eprintln!(
                "agalma: cannot open ledger {}: {err}",
                ledger_path.display()
            );
            return ExitCode::FAILURE;
        }
    }
    let _ = std::fs::remove_file(state_dir.join("STOP"));
    println!("agalma: kill latch cleared; dispatch may resume");
    ExitCode::SUCCESS
}

fn status_command() -> ExitCode {
    let state_dir = match crate::config::resolve_state_dir(None) {
        Ok(dir) => dir,
        Err(err) => {
            eprintln!("agalma: {err}");
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
    let executions = match ledger.executions() {
        Ok(executions) => executions,
        Err(err) => {
            eprintln!("agalma: cannot read executions: {err}");
            return ExitCode::FAILURE;
        }
    };
    if executions.is_empty() {
        println!("no executions");
    }
    for record in &executions {
        let parked = ledger.events(&record.execution_id).ok().and_then(|events| {
            events
                .iter()
                .rev()
                .find(|e| e.kind == "parked")
                .and_then(|e| e.payload.get("reason"))
                .and_then(|v| v.as_str())
                .map(str::to_string)
        });
        println!(
            "{} phase={:?} state={:?} attempt={} parked_reason={:?}",
            record.execution_id, record.phase, record.state, record.attempt, parked
        );
    }
    match ledger.kill_latch() {
        Ok(latched) => println!("kill_latch={latched}"),
        Err(err) => eprintln!("agalma: cannot read kill latch: {err}"),
    }
    ExitCode::SUCCESS
}

/// Best-effort exclusive lock file in the state dir.
///
/// A live pid holding the lock refuses a second conductor; a stale lock (dead
/// pid) is reclaimed. Removed on drop.
struct LockGuard {
    path: PathBuf,
}

impl LockGuard {
    fn acquire(state_dir: &std::path::Path) -> Result<Self, String> {
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
