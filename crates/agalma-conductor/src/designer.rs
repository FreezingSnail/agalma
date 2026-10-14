//! `agalma designer` — one scoped designer decomposition attempt for an existing
//! epic (CP.6).
//!
//! Runs exactly one designer attempt for **one** named open epic, sharing the
//! fleet sweep's per-epic semantics ([`crate::sweep::sweep_epic`]): the same
//! planner-route gate (a `route.model` decision, paid-only, never downgrading),
//! stop policy, durable attempt bookkeeping (`decomp-attempts` /
//! `decomp-inflight`), designer session, and CP.1 decomposition gate. It refuses
//! epics the sweep would never touch (missing / closed / human-escalated, or not
//! a decomposition candidate) instead of blindly replanning them, and it never
//! examines any other candidate epic.
//!
//! Mirrors [`crate::concierge::run_concierge_command`]: canonicalize the repo,
//! resolve the state dir, build the planner gate from the paid-credential state
//! plus the explicit `--model` override, fail closed (FAILURE, no session) when
//! the planner route is unavailable, start the credential-injecting provider
//! proxy only when the paid path is actually active, then run the live designer
//! session.
//!
//! Output contract (exit code):
//! - `GATE: OK (<N> staged child(ren))` and SUCCESS when the attempted epic's
//!   gate passed (or a recovered attempt passed);
//! - `decomp-rejected: ...` and FAILURE when the gate failed (rework required);
//! - the skip reason and FAILURE when the planner route was unavailable or a
//!   stop suppressed dispatch;
//! - an explicit `no attempt recorded` and FAILURE otherwise.

use std::path::{Path, PathBuf};
use std::process::ExitCode;

use agalma_taskqueue::{BdTaskQueue, STAGED_LABEL};

use crate::cli::DesignerArgs;
use crate::provider_proxy::{ProviderProxy, ProxyConfig};
use crate::sweep::{run_designer_with_stop, PlannerGate};
use crate::{config, routing};

/// `agalma designer --epic <id> [--repo <path>] [--state-dir <dir>] [--model <id>]`:
/// run one scoped designer attempt and report the gate outcome.
pub fn run_designer_command(args: &DesignerArgs) -> ExitCode {
    let repo = args.repo.clone().unwrap_or_else(|| PathBuf::from("."));
    let repo = match std::fs::canonicalize(&repo) {
        Ok(repo) => repo,
        Err(err) => {
            eprintln!("agalma: repo {}: {err}", repo.display());
            return ExitCode::FAILURE;
        }
    };
    let state_dir = match config::resolve_state_dir(args.state_dir.as_deref()) {
        Ok(dir) => dir,
        Err(err) => {
            eprintln!("agalma: {err}");
            return ExitCode::FAILURE;
        }
    };

    let paid_available = routing::paid_credential_available();
    let gate = PlannerGate::from_env(paid_available, args.model.as_deref());
    if !gate.available() {
        eprintln!("agalma: designer: {}", gate.unavailable_reason());
        return ExitCode::FAILURE;
    }

    // Stop admission BEFORE the proxy and the confined session are constructed
    // (`agalma-5fy.5`): Running/Stopped/Unavailable through the selected ledger
    // binding. An unreadable established ledger fails closed here, so the
    // provider proxy is never started and no designer session/scratch dir is
    // created. The same control is passed through the session seam.
    let stop = crate::stop::bind_stop_control(&state_dir);
    if let Some(reason) = stop.decision().reason() {
        eprintln!("agalma: designer: {reason}");
        return ExitCode::FAILURE;
    }

    // The paid planner route needs the credential-injecting proxy. Start it
    // only when the paid path is actually active.
    let allow_paid = routing::allow_paid_from_env();
    let proxy = if allow_paid && paid_available {
        match ProviderProxy::start(ProxyConfig::from_env()) {
            Ok(proxy) => Some(proxy),
            Err(err) => {
                eprintln!("agalma: designer: provider proxy: {err}");
                return ExitCode::FAILURE;
            }
        }
    } else {
        None
    };
    let proxy_url = proxy.as_ref().map(ProviderProxy::url);

    let report = match run_designer_with_stop(
        &repo,
        &state_dir,
        &args.epic,
        paid_available,
        proxy_url,
        args.model.as_deref(),
        stop,
    ) {
        Ok(report) => report,
        Err(err) => {
            eprintln!("agalma: designer: {err}");
            return ExitCode::FAILURE;
        }
    };

    if let Some(reason) = &report.skipped_reason {
        eprintln!("agalma: designer: {reason}");
        return ExitCode::FAILURE;
    }
    if report.stopped {
        eprintln!(
            "agalma: designer: stop policy active (STOP sentinel or kill latch); \
             dispatch suppressed mid-attempt"
        );
        return ExitCode::FAILURE;
    }
    if report.approved.iter().any(|epic| epic == &args.epic) {
        match staged_child_count(&repo, &args.epic) {
            Some(count) => println!("GATE: OK ({count} staged child(ren))"),
            // Best-effort: the gate passed, so a child-read failure (a
            // concurrent `bd` edit, a transient CLI error) must not turn a
            // success into a failure.
            None => println!("GATE: OK"),
        }
        if report.recovered.iter().any(|epic| epic == &args.epic) {
            println!(
                "recovered interrupted decomposition for {}; reconciled committed children",
                args.epic
            );
        }
        return ExitCode::SUCCESS;
    }
    if report.rejected.iter().any(|epic| epic == &args.epic) {
        eprintln!(
            "decomp-rejected: rework required; ordered violations are in the epic comment \
             and the decomp-fix task (run `agalma plan-check --epic {}`)",
            args.epic
        );
        return ExitCode::FAILURE;
    }
    if report.escalated.iter().any(|epic| epic == &args.epic) {
        eprintln!(
            "agalma: designer: {} escalated to a human (decomposition retries exhausted)",
            args.epic
        );
        return ExitCode::FAILURE;
    }
    eprintln!("agalma: designer: no attempt recorded for {}", args.epic);
    ExitCode::FAILURE
}

/// The number of `staged`-labelled children under `epic`, or `None` when the
/// children cannot be read (best-effort reporting only).
fn staged_child_count(repo: &Path, epic: &str) -> Option<usize> {
    let children = BdTaskQueue::new(repo).plan_children(epic).ok()?;
    Some(
        children
            .iter()
            .filter(|child| child.labels.iter().any(|label| label == STAGED_LABEL))
            .count(),
    )
}
