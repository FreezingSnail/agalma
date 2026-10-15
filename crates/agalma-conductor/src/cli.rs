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
    /// Set the kill latch: stop new claims/dispatch (human-only).
    Pause(StateArgs),
    /// Request cancellation of one execution (human-only; typed confirmation).
    Cancel(CancelArgs),
    /// Report ledger and execution status (TUI read-model snapshot).
    Status(StatusArgs),
    /// Live refresh loop over the TUI read model (read-only watch view).
    Watch(WatchArgs),
    /// Print the recorded digest for an execution (JSON + one-line summary).
    Digest(DigestArgs),
    /// Print the model routing table and the active routes (H3).
    Models,
    /// Validate an epic's staged decomposition (CP.1): schema, caps, tier, graph.
    PlanCheck(PlanCheckArgs),
    /// One-shot intake (CP.4): turn a goal into an epic + design via a planner session.
    Concierge(ConciergeArgs),
    /// Run one scoped designer decomposition attempt for an existing epic (CP.6).
    Designer(DesignerArgs),
    /// Reap terminal executions' heavy scratch (manual/older state dirs).
    Gc(GcArgs),
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
    /// Override the model identifier for the builder role only (SF.8). Outranks
    /// `--model` for the builder; other roles keep their existing selection.
    #[arg(long, value_name = "ID")]
    pub builder_model: Option<String>,
    /// Harness implementation to bind (`opencode|reference|reference-fail`).
    #[arg(long, value_name = "ID")]
    pub harness: Option<String>,
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

/// Arguments for `agalma cancel`.
#[derive(Debug, Args)]
pub struct CancelArgs {
    /// Execution id (`exec:<task>:<generation>`).
    #[arg(long, value_name = "ID")]
    pub execution: String,
    /// Typed confirmation; must be `cancel` for the request to apply.
    #[arg(long, value_name = "WORD")]
    pub confirm: Option<String>,
    /// Override the state directory.
    #[arg(long, value_name = "DIR")]
    pub state_dir: Option<PathBuf>,
}

/// Arguments for `agalma status`.
#[derive(Debug, Args)]
pub struct StatusArgs {
    /// Override the state directory (defaults to `AGALMA_STATE_DIR` or the
    /// platform application-support directory).
    #[arg(long, value_name = "DIR")]
    pub state_dir: Option<PathBuf>,
    /// Emit the read-model snapshot as JSON (TUI contract).
    #[arg(long)]
    pub json: bool,
}

/// Arguments for `agalma watch`.
#[derive(Debug, Args)]
pub struct WatchArgs {
    /// Override the state directory (defaults to `AGALMA_STATE_DIR` or the
    /// platform application-support directory).
    #[arg(long, value_name = "DIR")]
    pub state_dir: Option<PathBuf>,
    /// Refresh interval in milliseconds (default 1000).
    #[arg(long, value_name = "MS", default_value_t = 1000)]
    pub interval_ms: u64,
    /// Render a single frame and exit (non-interactive; never enters the
    /// alternate screen).
    #[arg(long)]
    pub once: bool,
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

/// Arguments for `agalma plan-check`.
#[derive(Debug, Args)]
pub struct PlanCheckArgs {
    /// Epic whose deferred/staged children are validated.
    #[arg(long, value_name = "ID")]
    pub epic: String,
    /// Repository (bd workspace) holding the backlog; defaults to the cwd.
    #[arg(long, value_name = "PATH")]
    pub repo: Option<PathBuf>,
    /// Override the minimum child count (`count-budget`).
    #[arg(long, value_name = "N")]
    pub min_children: Option<usize>,
    /// Minimum median dependency-wave width (`graph-width`).
    #[arg(long, value_name = "N")]
    pub min_width: Option<usize>,
}

/// Arguments for `agalma concierge`.
#[derive(Debug, Args)]
pub struct ConciergeArgs {
    /// The stated goal to turn into an epic + design.
    #[arg(value_name = "GOAL")]
    pub goal: String,
    /// Target repository (bd workspace); defaults to the cwd.
    #[arg(long, value_name = "PATH")]
    pub repo: Option<PathBuf>,
    /// Override the state directory.
    #[arg(long, value_name = "DIR")]
    pub state_dir: Option<PathBuf>,
    /// Override the model identifier.
    #[arg(long, value_name = "ID")]
    pub model: Option<String>,
}

/// Arguments for `agalma designer`.
#[derive(Debug, Args)]
pub struct DesignerArgs {
    /// Epic to decompose.
    #[arg(long, value_name = "ID")]
    pub epic: String,
    /// Target repository (bd workspace); defaults to the cwd.
    #[arg(long, value_name = "PATH")]
    pub repo: Option<PathBuf>,
    /// Override the state directory.
    #[arg(long, value_name = "DIR")]
    pub state_dir: Option<PathBuf>,
    /// Override the model identifier.
    #[arg(long, value_name = "ID")]
    pub model: Option<String>,
}

/// Arguments for `agalma gc`.
#[derive(Debug, Args)]
pub struct GcArgs {
    /// Override the state directory (defaults to `AGALMA_STATE_DIR` or the
    /// platform application-support directory).
    #[arg(long, value_name = "DIR")]
    pub state_dir: Option<PathBuf>,
}
