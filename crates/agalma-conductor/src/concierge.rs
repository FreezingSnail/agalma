//! `agalma concierge` — one-shot intake (CP.4, `agalma-i5h.5`).
//!
//! Turns a stated goal into a durable, designed epic by running **one**
//! concierge session through the shared harness seam with the canonical
//! [`crate::CONCIERGE_PROMPT`] content and the paid-only `planner` route
//! (recorded as a `route.model` decision, exactly as the CP.3 sweep does).
//!
//! The session authors the epic(s) and design(s) via `bd` itself — the
//! conductor never creates tickets. On completion the conductor parses the
//! session output contract (`epic:` / `design:` / `handoff:` lines), verifies
//! each epic exists in `bd` with a non-empty `--design`, and reports the epic
//! ids plus the next step (decomposition). It fails closed when the planner
//! route is unavailable (`AGALMA_ALLOW_PAID` off or no credential) with a clear
//! error and no session spawned — never a silent downgrade to a free builder
//! model.
//!
//! ## Testability
//!
//! [`run_concierge`] takes a `&dyn ConciergeSession`, so tests script the
//! session (create valid/invalid epics) without touching the harness.
//! [`LiveConciergeSession`] is the production implementation.

use std::path::{Path, PathBuf};
use std::process::{Command, ExitCode};

use agalma_contracts::ids::ArtifactRef;
use agalma_contracts::{ContractError, OperationId, TaskFamily};
use serde_json::Value;

use crate::cli::ConciergeArgs;
use crate::provider_proxy::{ProviderProxy, ProxyConfig};
use crate::session::{sanitize, LiveSession, SessionSpec};
use crate::stop::{FileStopControl, StopControl};
use crate::sweep::PlannerGate;
use crate::{config, routing};

/// The route/decision scope recorded for a concierge intake.
pub const CONCIERGE_SCOPE: &str = "concierge";
/// Next step printed once an epic's design exists.
pub const NEXT_STEP: &str = "Ready for decomposition (the epic sweep runs the designer).";

/// Inputs handed to one concierge session.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ConciergeRequest {
    /// The stated goal (rough idea) to turn into an epic + design.
    pub goal: String,
    /// Target repository (the bd workspace the concierge authors the epic in).
    pub repo: PathBuf,
    /// Conductor state dir (session scratch + ledger for the route decision).
    pub state_dir: PathBuf,
    /// Planner-route model id pinned for the session.
    pub model: String,
    /// Stable per-goal scratch key.
    pub run_key: String,
}

/// The concierge-session seam: production runs the OpenCode adapter; tests
/// script it. On success it returns the session output contract text.
pub trait ConciergeSession {
    /// Run one concierge session for `request`, returning its output contract.
    fn run(&self, request: &ConciergeRequest) -> Result<String, ContractError>;
}

/// What the intake produced, for reporting and tests.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct ConciergeReport {
    /// Epic ids the session authored (verified present with a design).
    pub epics: Vec<String>,
    /// `handoff:` lines from the session output contract.
    pub handoffs: Vec<String>,
}

/// Run one concierge intake against `repo`.
///
/// Stop-safe entry point: honors the production stop policy (`<state>/STOP`
/// sentinel or the ledger kill latch), so an explicit concierge intake never
/// launches a paid planning session while stopped (`agalma-r2l`). Tests that
/// script the control call [`run_concierge_with_stop`].
///
/// Fails closed when the planner route is unavailable, before any session is
/// spawned. Records the `route.model` decision (class `planner`) through the
/// shared routing seam.
pub fn run_concierge(
    repo: &Path,
    state_dir: &Path,
    goal: &str,
    gate: &PlannerGate,
    session: &dyn ConciergeSession,
) -> Result<ConciergeReport, ContractError> {
    run_concierge_with_stop(
        repo,
        state_dir,
        goal,
        gate,
        session,
        &FileStopControl::new(state_dir),
    )
}

/// Run one concierge intake honoring an explicit stop control.
///
/// The stop policy is applied before the planner-route resolution and before
/// the session runs: while stopped, no route decision is recorded and no
/// session is spawned. The control is never cleared here.
pub fn run_concierge_with_stop(
    repo: &Path,
    state_dir: &Path,
    goal: &str,
    gate: &PlannerGate,
    session: &dyn ConciergeSession,
    stop: &dyn StopControl,
) -> Result<ConciergeReport, ContractError> {
    if stop.stopped() {
        return Err(ContractError::KnownFailure(
            "stop policy active (STOP sentinel or kill latch); concierge intake suppressed"
                .to_string(),
        ));
    }
    if !gate.available() {
        return Err(ContractError::KnownFailure(gate.unavailable_reason()));
    }

    let run_key = run_key(goal);
    // Resolve (and durably record) the planner route for this intake.
    let route = routing::resolve_route(
        state_dir,
        CONCIERGE_SCOPE,
        "planner",
        TaskFamily::Feature,
        gate.allow_paid,
        gate.paid_available,
        gate.override_model.as_deref(),
    )?;

    let request = ConciergeRequest {
        goal: goal.to_string(),
        repo: repo.to_path_buf(),
        state_dir: state_dir.to_path_buf(),
        model: route.model.clone(),
        run_key,
    };
    eprintln!(
        "agalma: concierge: running intake session (model {})",
        route.model
    );
    let output = session.run(&request)?;
    let contract = parse_contract(&output)?;

    let mut report = ConciergeReport {
        epics: Vec::new(),
        handoffs: contract.handoffs,
    };
    for epic in &contract.epics {
        verify_epic(repo, epic)?;
        report.epics.push(epic.clone());
    }
    Ok(report)
}

/// `agalma concierge "<goal>" [--repo <path>]`: run one live intake and report.
pub fn run_concierge_command(args: &ConciergeArgs) -> ExitCode {
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
        eprintln!("agalma: concierge: {}", gate.unavailable_reason());
        return ExitCode::FAILURE;
    }

    // The paid planner route needs the credential-injecting proxy. Start it
    // only when the paid path is actually active.
    let allow_paid = routing::allow_paid_from_env();
    let proxy = if allow_paid && paid_available {
        match ProviderProxy::start(ProxyConfig::from_env()) {
            Ok(proxy) => Some(proxy),
            Err(err) => {
                eprintln!("agalma: concierge: provider proxy: {err}");
                return ExitCode::FAILURE;
            }
        }
    } else {
        None
    };
    let proxy_url = proxy.as_ref().map(ProviderProxy::url);

    let session = match LiveConciergeSession::build(&state_dir, proxy_url, paid_available) {
        Ok(session) => session,
        Err(err) => {
            eprintln!("agalma: concierge: {err}");
            return ExitCode::FAILURE;
        }
    };

    match run_concierge(&repo, &state_dir, &args.goal, &gate, &session) {
        Ok(report) => {
            for epic in &report.epics {
                println!("epic: {epic}");
            }
            for handoff in &report.handoffs {
                println!("{handoff}");
            }
            println!("{NEXT_STEP}");
            ExitCode::SUCCESS
        }
        Err(err) => {
            eprintln!("agalma: concierge: {err}");
            ExitCode::FAILURE
        }
    }
}

// ---------------------------------------------------------------------------
// Output contract
// ---------------------------------------------------------------------------

/// The parsed session output contract.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
struct Contract {
    epics: Vec<String>,
    handoffs: Vec<String>,
}

/// Parse the `epic:` / `handoff:` lines from the session output. A missing
/// `epic:` line is a hard error: the intake produced nothing to design against.
fn parse_contract(output: &str) -> Result<Contract, ContractError> {
    let mut contract = Contract::default();
    for line in output.lines() {
        let line = line.trim();
        if let Some(rest) = line.strip_prefix("epic:") {
            let id = rest.trim();
            if !id.is_empty() {
                contract.epics.push(id.to_string());
            }
        } else if let Some(rest) = line.strip_prefix("handoff:") {
            let handoff = rest.trim();
            if !handoff.is_empty() {
                contract.handoffs.push(handoff.to_string());
            }
        }
    }
    if contract.epics.is_empty() {
        return Err(ContractError::KnownFailure(
            "concierge output contract missing `epic:` line (no epic authored)".to_string(),
        ));
    }
    Ok(contract)
}

/// Verify an epic exists in the repo's bd workspace with a non-empty design.
fn verify_epic(repo: &Path, id: &str) -> Result<(), ContractError> {
    let out = Command::new("bd")
        .arg("--db")
        .arg(repo.join(".beads"))
        .args(["show", id, "--json", "--include-comments"])
        .current_dir(repo)
        .env("BD_NON_INTERACTIVE", "1")
        .env("GIT_TERMINAL_PROMPT", "0")
        .output()
        .map_err(|e| ContractError::KnownFailure(format!("bd show {id}: {e}")))?;
    if !out.status.success() {
        return Err(ContractError::KnownFailure(format!(
            "concierge epic {id} not found: {}",
            String::from_utf8_lossy(&out.stderr).trim()
        )));
    }
    let value: Value = serde_json::from_slice(&out.stdout)
        .map_err(|e| ContractError::KnownFailure(format!("bd show {id}: invalid JSON: {e}")))?;
    let issue = value
        .as_array()
        .and_then(|issues| issues.first())
        .ok_or_else(|| ContractError::KnownFailure(format!("concierge epic {id} not found")))?;
    let design = issue
        .get("design")
        .and_then(Value::as_str)
        .unwrap_or("")
        .trim();
    if design.is_empty() {
        return Err(ContractError::KnownFailure(format!(
            "concierge epic {id} has no design (`bd update {id} --design-file <file>`)"
        )));
    }
    Ok(())
}

/// Stable per-goal scratch key (hex FNV-1a).
fn run_key(goal: &str) -> String {
    let mut hash: u64 = 0xcbf2_9ce4_8422_2325;
    for byte in goal.as_bytes() {
        hash ^= *byte as u64;
        hash = hash.wrapping_mul(0x0000_0100_0000_01b3);
    }
    format!("{hash:016x}")
}

// ---------------------------------------------------------------------------
// Live concierge session (OpenCode adapter)
// ---------------------------------------------------------------------------

/// Production [`ConciergeSession`]: one confined OpenCode session in the target
/// repo with the concierge prompt and the planner-route model.
pub struct LiveConciergeSession {
    live: LiveSession,
}

impl LiveConciergeSession {
    /// Assemble the live session config (materializing the sandbox profile).
    pub fn build(
        state_dir: &Path,
        proxy_url: Option<String>,
        paid_available: bool,
    ) -> Result<Self, ContractError> {
        Ok(LiveConciergeSession {
            live: LiveSession::build(state_dir, proxy_url, paid_available)?,
        })
    }
}

impl ConciergeSession for LiveConciergeSession {
    fn run(&self, request: &ConciergeRequest) -> Result<String, ContractError> {
        let key = sanitize(&request.run_key);
        let dir_key = format!("concierge/{key}");
        // The per-intake prompt is read by the adapter on the parent
        // filesystem; the goal file and the output contract are read/written by
        // the confined session, so they live under the session's attempt root
        // (conductor-owned, outside the target repo — agalma-22f).
        let scratch = request.state_dir.join("concierge").join(&key);
        std::fs::create_dir_all(&scratch).map_err(|e| {
            ContractError::KnownFailure(format!("mkdir {}: {e}", scratch.display()))
        })?;
        let attempt_scratch =
            crate::session::attempt_root(&request.state_dir, &dir_key).join("intake");
        std::fs::create_dir_all(&attempt_scratch).map_err(|e| {
            ContractError::KnownFailure(format!("mkdir {}: {e}", attempt_scratch.display()))
        })?;
        let goal_path = attempt_scratch.join("goal.txt");
        std::fs::write(&goal_path, &request.goal).map_err(|e| {
            ContractError::KnownFailure(format!("write goal {}: {e}", goal_path.display()))
        })?;
        let output_path = attempt_scratch.join("output.txt");
        let prompt_path = scratch.join("prompt.md");
        std::fs::write(&prompt_path, intake_prompt(&goal_path, &output_path)).map_err(|e| {
            ContractError::KnownFailure(format!(
                "write concierge prompt {}: {e}",
                prompt_path.display()
            ))
        })?;

        let spec = SessionSpec {
            role: "concierge".to_string(),
            model: request.model.clone(),
            repo: request.repo.clone(),
            dir_key,
            socket_name: format!("harness-concierge-{key}.sock"),
            operation_id: OperationId::new(format!("op:concierge:{key}")),
            prompt_path,
            input_ref: ArtifactRef::derive("concierge-turn"),
            tool_policy_ref: ArtifactRef::derive("tool-policy-concierge"),
            genome_ref: ArtifactRef::derive("genome"),
            constraints_ref: ArtifactRef::derive("platform-constraints"),
            output_path: Some(output_path),
            error_label: "concierge session".to_string(),
        };
        let result = crate::session::run_session(&self.live, &spec);
        // The attempt-root scratch is conductor-owned; remove it best-effort.
        let _ = std::fs::remove_dir_all(&attempt_scratch);
        Ok(result?.unwrap_or_default())
    }
}

/// Compose the per-intake prompt: the canonical concierge prompt plus the exact
/// goal and output-contract file paths (the session reads the goal file and
/// writes its contract before finishing).
fn intake_prompt(goal_path: &Path, output_path: &Path) -> String {
    format!(
        "{prompt}\n## Intake\n\n\
         Goal file: {goal}\nRead it before answering.\n\n\
         When the epic(s) and design(s) exist, write your output contract to the \
         file {output} as well as emitting it:\n\n\
         ```\nepic: <epic-id>\ndesign: written to <epic-id> --design\nhandoff: bd show <epic-id>\n```\n",
        prompt = crate::CONCIERGE_PROMPT,
        goal = goal_path.display(),
        output = output_path.display(),
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_contract_collects_epic_and_handoff_lines() {
        let output = "noise\nepic: agalma-abc\nepic: agalma-def\ndesign: written\nhandoff: bd show agalma-abc\n";
        let contract = parse_contract(output).expect("contract");
        assert_eq!(contract.epics, vec!["agalma-abc", "agalma-def"]);
        assert_eq!(contract.handoffs, vec!["bd show agalma-abc"]);
    }

    #[test]
    fn parse_contract_rejects_missing_epic_line() {
        let err = parse_contract("design: written\nhandoff: bd show x\n").unwrap_err();
        assert!(err.to_string().contains("epic:"), "{err}");
    }

    #[test]
    fn run_key_is_stable_and_distinct() {
        assert_eq!(run_key("stand up a TUI"), run_key("stand up a TUI"));
        assert_ne!(run_key("a"), run_key("b"));
    }
}
