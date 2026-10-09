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
    /// Clear the kill latch and resume dispatch (human-only).
    Resume(StateArgs),
    /// Report ledger and execution status.
    Status(StateArgs),
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
