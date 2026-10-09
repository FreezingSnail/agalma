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
    /// Run the conductor against a fixture task.
    Run(RunArgs),
    /// Clear the kill latch and resume dispatch (human-only).
    Resume,
    /// Report ledger and execution status.
    Status,
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
