//! Command-line interface (`agalma run|resume|status`).

use std::path::PathBuf;

use clap::{Args, Parser, Subcommand};

/// Agalma conductor.
#[derive(Debug, Parser)]
#[command(name = "agalma", version, about = "Agalma conductor")]
pub struct Cli {
    #[command(subcommand)]
    pub command: Command,
}

/// Conductor subcommands.
#[derive(Debug, Subcommand)]
pub enum Command {
    /// Run (or resume) the conductor against a fixture task.
    Run(RunArgs),
    /// Queue-driven work loop over a real repository (M1).
    Work(WorkArgs),
    /// Reconcile the bd projection against the ledger (ledger wins).
    Reconcile(ReconcileArgs),
    /// Clear the kill latch and resume dispatch (human-only).
    Resume(StateArgs),
    /// Report ledger and execution status.
    Status(StateArgs),
    /// Print the recorded digest for an execution (JSON + one-line summary).
    Digest(DigestArgs),
}

/// Arguments for `agalma work`.
#[derive(Debug, Args)]
pub struct WorkArgs {
    /// Origin repository; also the bd workspace for the task backlog (M1).
    #[arg(long, value_name = "PATH")]
    pub repo: PathBuf,
    /// Execute a single task and exit (0 on done, nonzero on park).
    #[arg(long)]
    pub once: bool,
    /// Process at most this many tasks before exiting.
    #[arg(long, value_name = "N")]
    pub max_tasks: Option<u32>,
    /// Override the state directory.
    #[arg(long, value_name = "DIR")]
    pub state_dir: Option<PathBuf>,
    /// Override the model identifier.
    #[arg(long, value_name = "ID")]
    pub model: Option<String>,
}

/// Arguments for `agalma reconcile`.
#[derive(Debug, Args)]
pub struct ReconcileArgs {
    /// Origin repository; also the bd workspace for the task backlog (M1).
    #[arg(long, value_name = "PATH")]
    pub repo: PathBuf,
    /// Override the state directory.
    #[arg(long, value_name = "DIR")]
    pub state_dir: Option<PathBuf>,
}

/// Arguments for `agalma run`.
#[derive(Debug, Args)]
pub struct RunArgs {
    /// Fixture repository containing the hardcoded task.
    #[arg(long, value_name = "PATH")]
    pub fixture: PathBuf,
    /// Execute a single pass and exit (M0 smoke path).
    #[arg(long)]
    pub once: bool,
    /// Override the state directory.
    #[arg(long, value_name = "DIR")]
    pub state_dir: Option<PathBuf>,
    /// Override the model identifier.
    #[arg(long, value_name = "ID")]
    pub model: Option<String>,
}

/// Arguments shared by the human `resume`/`status` subcommands.
#[derive(Debug, Args)]
pub struct StateArgs {
    /// Override the state directory (defaults to `AGALMA_STATE_DIR` or the
    /// platform application-support directory).
    #[arg(long, value_name = "DIR")]
    pub state_dir: Option<PathBuf>,
}

/// Arguments for `agalma digest`.
#[derive(Debug, Args)]
pub struct DigestArgs {
    /// Execution id (`exec:<task>:<generation>`).
    #[arg(long, value_name = "ID")]
    pub execution: String,
    /// Override the state directory.
    #[arg(long, value_name = "DIR")]
    pub state_dir: Option<PathBuf>,
}
