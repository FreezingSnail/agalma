//! `agalma plan-check` (CP.1, `agalma-i5h.2`).
//!
//! Deterministic decomposition validation for an epic's staged children: reads
//! the children (including deferred/staged ones) from the bd workspace, runs the
//! [`agalma_taskqueue::decomp`] validator, and prints `OK` (exit 0) or a
//! numbered rule-named violation list (exit non-zero). The sweep gate (CP.3)
//! consumes this command.

use std::path::PathBuf;
use std::process::ExitCode;

use agalma_contracts::TaskId;
use agalma_taskqueue::{decomp, BdTaskQueue};

use crate::cli::PlanCheckArgs;

/// Run `agalma plan-check`; map read failures and violations to a non-zero exit.
pub fn run_plan_check(args: &PlanCheckArgs) -> ExitCode {
    let repo = args.repo.clone().unwrap_or_else(|| PathBuf::from("."));
    let repo = match std::fs::canonicalize(&repo) {
        Ok(repo) => repo,
        Err(err) => {
            eprintln!("agalma: repo {}: {err}", repo.display());
            return ExitCode::FAILURE;
        }
    };

    let queue = BdTaskQueue::new(repo.as_path());
    let comments = match queue.projection(&TaskId::new(args.epic.as_str())) {
        Ok(projection) => projection.comments,
        Err(err) => {
            eprintln!("agalma: epic {}: {err}", args.epic);
            return ExitCode::FAILURE;
        }
    };
    let children = match queue.plan_children(&args.epic) {
        Ok(children) => children,
        Err(err) => {
            eprintln!("agalma: cannot read children of {}: {err}", args.epic);
            return ExitCode::FAILURE;
        }
    };

    let params = decomp::ValidateParams {
        allow_below_min: decomp::has_human_verdict(&comments),
        min_children: args.min_children.unwrap_or(decomp::MIN_CHILDREN),
        min_width: args.min_width.unwrap_or(1),
    };
    let violations = decomp::validate(&children, &params);

    if violations.is_empty() {
        println!("OK");
        ExitCode::SUCCESS
    } else {
        println!(
            "{} violation(s) in decomposition of {}:",
            violations.len(),
            args.epic
        );
        for (index, violation) in violations.iter().enumerate() {
            println!("{}. {}: {}", index + 1, violation.rule, violation.detail);
        }
        ExitCode::FAILURE
    }
}
