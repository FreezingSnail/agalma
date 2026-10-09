//! Agalma conductor.
//!
//! Composition root for the Agalma factory. Dependency direction is
//! `conductor -> implementations -> contracts`; see `composition` for the wiring
//! points and the architecture lint in `tests/architecture.rs` for the
//! enforcement of impl-to-impl isolation.

pub mod cli;
pub mod composition;
pub mod config;
pub mod provider_proxy;

use std::process::ExitCode;

use clap::Parser;

use crate::cli::{Cli, Command, RunArgs};
use crate::config::{Config, PHASE_PLAN};

/// Parse the CLI and run the conductor.
pub async fn run() -> ExitCode {
    let cli = Cli::parse();
    match cli.command {
        Command::Run(args) => run_command(args),
        Command::Resume => placeholder("resume"),
        Command::Status => placeholder("status"),
    }
}

fn run_command(args: RunArgs) -> ExitCode {
    let config = match Config::resolve(&args) {
        Ok(config) => config,
        Err(err) => {
            eprintln!("agalma: {err}");
            return ExitCode::FAILURE;
        }
    };

    if args.once {
        print_plan(&config);
        return ExitCode::SUCCESS;
    }

    // TODO(M0.7): start the serial dispatch loop and serve until stopped.
    if let Err(err) = composition::Composition::build() {
        eprintln!("agalma: composition root not wired yet: {err}");
    }
    placeholder("run")
}

fn print_plan(config: &Config) {
    println!("agalma run --once");
    println!("fixture:   {}", config.fixture.display());
    println!("state_dir: {}", config.state_dir.display());
    println!("model:     {}", config.model);
    println!("phases:    {}", PHASE_PLAN.join(" -> "));
    println!("TODO(M0.7): wire the composition root and execute the run loop");
}

fn placeholder(command: &str) -> ExitCode {
    eprintln!("agalma: `{command}` is not implemented yet (TODO: M0.7 daemon loop)");
    ExitCode::FAILURE
}
