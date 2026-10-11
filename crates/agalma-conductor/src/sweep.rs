//! Epic sweep + staged decomposition gate (CP.3, `agalma-i5h.4`).
//!
//! After boot recovery and before ordinary claiming, the work loop runs one
//! serial sweep over the backlog's open epics (design:
//! `docs/planning-pipeline.md` §"Epic sweep"):
//!
//! 1. **detect** — open epics with zero children, or epics whose latest
//!    decomposition is `decomp-rejected` with retries remaining (< 3). Epics
//!    already escalated to a human (`human` label) are ignored.
//! 2. **plan** — resolve the paid-only `planner` route (recorded as a
//!    `route.model` decision). When the planner route is unavailable
//!    (`AGALMA_ALLOW_PAID` off or no provider credential) the sweep skips with a
//!    clear log line and never downgrades to a free builder model.
//! 3. **design** — run one designer session through the harness seam
//!    ([`DesignerSession`]); the designer authors children via `bd` itself, the
//!    conductor does not.
//! 4. **gate** — run the CP.1 [`decomp::validate`] over the staged children.
//!    Pass: label them `decomp-approved` (they stay deferred/staged — approval is
//!    a human `bd undefer`). Fail: label them `decomp-rejected`, create ONE P1
//!    `decomp-fix` task under the epic carrying the ordered rule-named violation
//!    list, and comment on the epic. The third failure escalates the epic to a
//!    human.
//!
//! Retry counting: the number of prior `decomp-fix` tasks under the epic (each
//! failed gate creates exactly one). An epic is a candidate while that count is
//! below [`MAX_DECOMP_RETRIES`]; the failure that reaches the bound also adds the
//! `human` label and stops the epic from being re-swept.
//!
//! ## Testability
//!
//! [`sweep_epics`] takes a `&dyn DesignerSession`, so tests script the designer
//! (create a valid/invalid decomposition, or do nothing) without touching the
//! harness. [`LiveDesignerSession`] is the production implementation.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::time::Duration;

use agalma_contracts::harness::{
    CreateSessionRequest, HarnessApi, Limits, OperationState, RunTurnRequest, StartAttemptRequest,
};
use agalma_contracts::ids::ArtifactRef;
use agalma_contracts::{ContractError, OperationId, TaskFamily, TaskId, TaskQueueApi};
use agalma_harness_opencode::{build_config, OpenCodeConfigInit};
use agalma_taskqueue::{decomp, BdTaskQueue, DECOMP_FIX_LABEL, HUMAN_LABEL, STAGED_LABEL};

use crate::harness_process::{self, HarnessTransport, OpenCodeHarnessInit};
use crate::phases::{ensure_cargo_home, poll_terminal, PollOutcome};
use crate::sandbox_process::SandboxSource;
use crate::routing;

/// Label applied to children a passing decomposition gate accepted.
pub const DECOMP_APPROVED: &str = "decomp-approved";
/// Label applied to children a failing decomposition gate rejected.
pub const DECOMP_REJECTED: &str = "decomp-rejected";
/// Priority of the single rework task created on a failed gate (P1).
pub const DECOMP_FIX_PRIORITY: u8 = 1;
/// Defer date for the rework task: it is a designer/human artifact, never
/// ordinary fleet work, so it is hidden from `bd ready` by construction.
pub const DECOMP_FIX_DEFER: &str = "2099-01-01";
/// Number of failed decomposition gates tolerated before human escalation.
pub const MAX_DECOMP_RETRIES: usize = 3;

/// The planner route gate: paid opt-in plus a present provider credential (or an
/// explicit `--model`/`AGALMA_MODEL` override).
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct PlannerGate {
    pub allow_paid: bool,
    pub paid_available: bool,
    pub override_model: Option<String>,
}

impl PlannerGate {
    /// Build from the loop's captured paid state and the explicit model override.
    pub fn new(allow_paid: bool, paid_available: bool, override_model: Option<&str>) -> Self {
        PlannerGate {
            allow_paid,
            paid_available,
            override_model: override_model
                .map(str::trim)
                .filter(|s| !s.is_empty())
                .map(str::to_string),
        }
    }

    /// Read the paid opt-in from the environment.
    pub fn from_env(paid_available: bool, override_model: Option<&str>) -> Self {
        PlannerGate::new(
            routing::allow_paid_from_env(),
            paid_available,
            override_model,
        )
    }

    /// Whether the planner route has an eligible candidate (override, or a paid
    /// opt-in plus a credential).
    pub fn available(&self) -> bool {
        self.override_model.is_some()
            || !routing::eligible_paid(
                routing::RouteClass::Planner,
                self.allow_paid,
                self.paid_available,
            )
            .is_empty()
    }

    /// A clear, single-line reason the planner route is unavailable.
    pub fn unavailable_reason(&self) -> String {
        if self.override_model.is_some() {
            return "override model configured".to_string();
        }
        if !self.allow_paid {
            format!(
                "planner route unavailable: {}=off (never downgrading to a free builder model)",
                routing::ALLOW_PAID_ENV
            )
        } else if !self.paid_available {
            "planner route unavailable: no paid provider credential present (never downgrading to a free builder model)".to_string()
        } else {
            "planner route unavailable: no eligible planner candidates".to_string()
        }
    }
}

/// Inputs handed to one designer session.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct DesignerRequest {
    /// Epic the designer decomposes.
    pub epic: String,
    /// Target repository (the bd workspace the designer authors children in).
    pub repo: PathBuf,
    /// Conductor state dir (session scratch).
    pub state_dir: PathBuf,
    /// Planner-route model id pinned for the session.
    pub model: String,
    /// The `designer.md` prompt file the session reads.
    pub prompt_path: PathBuf,
}

/// The designer-session seam: production runs the OpenCode adapter; tests script
/// it. The designer authors children via `bd` itself.
pub trait DesignerSession {
    /// Run one designer session for `request`.
    fn run(&self, request: &DesignerRequest) -> Result<(), ContractError>;
}

/// What the sweep did, for logging and tests.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct SweepReport {
    /// Set when the whole sweep was skipped (planner unavailable).
    pub skipped_reason: Option<String>,
    /// Candidate epics the sweep examined.
    pub epics_seen: Vec<String>,
    /// Epics a designer session completed for.
    pub epics_swept: Vec<String>,
    /// Epics whose gate passed.
    pub approved: Vec<String>,
    /// Epics whose gate failed.
    pub rejected: Vec<String>,
    /// Epics escalated to a human (retries exhausted).
    pub escalated: Vec<String>,
}

/// Run one serial sweep over the open epics in `repo`.
///
/// Never spawns more than one designer session at a time (serial MVP, cap 1).
/// Returns a [`SweepReport`]; the caller decides whether to surface it.
pub fn sweep_epics(
    repo: &Path,
    state_dir: &Path,
    gate: &PlannerGate,
    designer: &dyn DesignerSession,
) -> Result<SweepReport, ContractError> {
    let mut report = SweepReport::default();
    if !gate.available() {
        let reason = gate.unavailable_reason();
        eprintln!("agalma: epic sweep skipped: {reason}");
        report.skipped_reason = Some(reason);
        return Ok(report);
    }

    let prompt_path = write_designer_prompt(state_dir)?;
    let mut queue = BdTaskQueue::new(repo);
    let epics = queue.open_epics()?;
    for epic in epics {
        if !is_candidate(&queue, &epic.id)? {
            continue;
        }
        report.epics_seen.push(epic.id.clone());

        // Resolve (and durably record) the planner route for this epic.
        let route = match routing::resolve_route(
            state_dir,
            &epic.id,
            "planner",
            TaskFamily::Feature,
            gate.allow_paid,
            gate.paid_available,
            gate.override_model.as_deref(),
        ) {
            Ok(route) => route,
            Err(err) => {
                eprintln!(
                    "agalma: epic sweep: planner route unavailable for {}: {err}",
                    epic.id
                );
                continue;
            }
        };

        let request = DesignerRequest {
            epic: epic.id.clone(),
            repo: repo.to_path_buf(),
            state_dir: state_dir.to_path_buf(),
            model: route.model.clone(),
            prompt_path: prompt_path.clone(),
        };
        eprintln!(
            "agalma: epic sweep: running designer for {} (model {})",
            epic.id, route.model
        );
        if let Err(err) = designer.run(&request) {
            eprintln!(
                "agalma: epic sweep: designer session for {} failed: {err}",
                epic.id
            );
            continue;
        }
        report.epics_swept.push(epic.id.clone());
        apply_gate(&mut queue, &epic.id, &mut report)?;
    }
    Ok(report)
}

/// Production entry point: build the planner gate and the live OpenCode designer
/// session, then sweep. Skips (with a log line) when the planner route is
/// unavailable, before constructing any session.
pub fn run_sweep(
    repo: &Path,
    state_dir: &Path,
    paid_available: bool,
    proxy_url: Option<String>,
    override_model: Option<&str>,
) -> Result<SweepReport, ContractError> {
    let gate = PlannerGate::from_env(paid_available, override_model);
    if !gate.available() {
        let reason = gate.unavailable_reason();
        eprintln!("agalma: epic sweep skipped: {reason}");
        return Ok(SweepReport {
            skipped_reason: Some(reason),
            ..SweepReport::default()
        });
    }
    let session = LiveDesignerSession::build(state_dir, proxy_url, paid_available)?;
    sweep_epics(repo, state_dir, &gate, &session)
}

/// A candidate epic: zero children, or rejected children with retries left.
fn is_candidate(queue: &BdTaskQueue, epic: &str) -> Result<bool, ContractError> {
    let children = queue.plan_children(epic)?;
    if children.is_empty() {
        return Ok(true);
    }
    let staged: Vec<&decomp::ChildTask> = children
        .iter()
        .filter(|child| child.labels.iter().any(|l| l == STAGED_LABEL))
        .collect();
    let rejected = staged
        .iter()
        .any(|child| child.labels.iter().any(|l| l == DECOMP_REJECTED));
    let approved = staged
        .iter()
        .any(|child| child.labels.iter().any(|l| l == DECOMP_APPROVED));
    let fixes = children
        .iter()
        .filter(|child| child.labels.iter().any(|l| l == DECOMP_FIX_LABEL))
        .count();
    Ok(rejected && !approved && fixes < MAX_DECOMP_RETRIES)
}

/// Run the CP.1 validator over the epic's staged children and project the gate
/// verdict onto the backlog.
fn apply_gate(
    queue: &mut BdTaskQueue,
    epic: &str,
    report: &mut SweepReport,
) -> Result<(), ContractError> {
    let all = queue.plan_children(epic)?;
    let staged: Vec<decomp::ChildTask> = all
        .iter()
        .filter(|child| child.labels.iter().any(|l| l == STAGED_LABEL))
        .cloned()
        .collect();
    let comments = queue.projection(&TaskId::new(epic))?.comments;
    let params = decomp::ValidateParams {
        allow_below_min: decomp::has_human_verdict(&comments),
        min_children: decomp::MIN_CHILDREN,
        min_width: 1,
    };
    let violations = decomp::validate(&staged, &params);

    if violations.is_empty() {
        for child in &staged {
            queue.add_label(&TaskId::new(&child.id), DECOMP_APPROVED)?;
        }
        queue.comment(
            &TaskId::new(epic),
            &format!(
                "decomp-approved: {} staged child(ren) passed validation; awaiting human `bd undefer`",
                staged.len()
            ),
        )?;
        report.approved.push(epic.to_string());
        return Ok(());
    }

    for child in &staged {
        queue.add_label(&TaskId::new(&child.id), DECOMP_REJECTED)?;
    }
    let prior = all
        .iter()
        .filter(|child| child.labels.iter().any(|l| l == DECOMP_FIX_LABEL))
        .count();
    let description = render_fix_description(epic, &violations);
    let fix_id = queue.create_task(
        &format!("decomp-fix: {epic}"),
        &description,
        Some(epic),
        DECOMP_FIX_PRIORITY,
        &[DECOMP_FIX_LABEL],
        Some(DECOMP_FIX_DEFER),
    )?;
    queue.comment(
        &TaskId::new(epic),
        &format!(
            "decomp-rejected: {} violation(s); rework task {fix_id}",
            violations.len()
        ),
    )?;
    report.rejected.push(epic.to_string());

    let failures = prior + 1;
    if failures >= MAX_DECOMP_RETRIES {
        queue.add_label(&TaskId::new(epic), HUMAN_LABEL)?;
        queue.comment(
            &TaskId::new(epic),
            &format!(
                "decomp retries exhausted ({failures}/{MAX_DECOMP_RETRIES}); escalating to a human"
            ),
        )?;
        eprintln!(
            "agalma: epic sweep: {epic} escalated to a human after {failures} failed decomposition(s)"
        );
        report.escalated.push(epic.to_string());
    }
    Ok(())
}

/// The ordered, rule-named violation list carried by a `decomp-fix` task.
fn render_fix_description(epic: &str, violations: &[decomp::Violation]) -> String {
    let mut out = String::new();
    out.push_str(&format!(
        "Decomposition rework required for epic {epic}.\n\n\
         `agalma plan-check --epic {epic}` failed with {} violation(s). Fix the child \
         decomposition so the validator passes; do not weaken the rules.\n\n\
         Ordered violations:\n",
        violations.len()
    ));
    for (index, violation) in violations.iter().enumerate() {
        out.push_str(&format!(
            "{}. {}: {}\n",
            index + 1,
            violation.rule,
            violation.detail
        ));
    }
    out.push_str(
        "\nThe epic sweep re-runs the designer once the children are revised.\n",
    );
    out
}

/// Materialize the canonical `designer.md` prompt under the state dir and return
/// its path.
fn write_designer_prompt(state_dir: &Path) -> Result<PathBuf, ContractError> {
    let dir = state_dir.join("prompts");
    std::fs::create_dir_all(&dir).map_err(|e| {
        ContractError::KnownFailure(format!("cannot create prompts dir {}: {e}", dir.display()))
    })?;
    let path = dir.join("designer.md");
    std::fs::write(&path, crate::DESIGNER_PROMPT).map_err(|e| {
        ContractError::KnownFailure(format!("cannot write designer prompt {}: {e}", path.display()))
    })?;
    Ok(path)
}

// ---------------------------------------------------------------------------
// Live designer session (OpenCode adapter)
// ---------------------------------------------------------------------------

/// Production [`DesignerSession`]: one confined OpenCode session per epic, run
/// in the target repo with the `designer.md` prompt and the planner-route model.
pub struct LiveDesignerSession {
    state_dir: PathBuf,
    profile_path: PathBuf,
    proxy_url: Option<String>,
    paid_proxy: bool,
    extra_ro_roots: Vec<PathBuf>,
    toolchain_env: BTreeMap<String, String>,
    turn_timeout: Duration,
    idle_timeout: Duration,
}

impl LiveDesignerSession {
    /// Assemble the live session config (materializing the sandbox profile).
    pub fn build(
        state_dir: &Path,
        proxy_url: Option<String>,
        paid_available: bool,
    ) -> Result<Self, ContractError> {
        let profile_path = state_dir.join("worker.sb");
        std::fs::write(&profile_path, agalma_sandbox::PRODUCT_PROFILE).map_err(|e| {
            ContractError::KnownFailure(format!(
                "cannot write sandbox profile {}: {e}",
                profile_path.display()
            ))
        })?;
        let (extra_ro_roots, toolchain_env) = crate::composition::toolchain()?;
        Ok(LiveDesignerSession {
            state_dir: state_dir.to_path_buf(),
            profile_path,
            proxy_url,
            paid_proxy: paid_available && routing::allow_paid_from_env(),
            extra_ro_roots,
            toolchain_env,
            turn_timeout: crate::composition::DEFAULT_TURN_TIMEOUT,
            idle_timeout: crate::composition::resolve_idle_timeout(),
        })
    }
}

impl DesignerSession for LiveDesignerSession {
    fn run(&self, request: &DesignerRequest) -> Result<(), ContractError> {
        let root = self
            .state_dir
            .join("sweep")
            .join(sanitize(&request.epic));
        let protected = canonical_dir(&root.join("protected"))?;
        let sock = canonical_dir(&root.join("sock"))?;

        // The designer authors tickets via `bd` in the target repo, so the repo
        // is the sandbox ATTEMPT root (writable); opencode's ephemeral state
        // lives under `protected`/`sock` and the repo-local XDG dirs.
        let attempt_dir = request.repo.clone();

        let mut env = self.toolchain_env.clone();
        let cargo_home = ensure_cargo_home(&root)
            .map_err(|e| ContractError::KnownFailure(format!("cargo home: {e}")))?;
        env.insert(
            "CARGO_HOME".to_string(),
            cargo_home.to_string_lossy().into_owned(),
        );
        // `bd` must be reachable inside the sandbox: prepend its directory to
        // PATH and expose that directory read-only.
        let mut extra_ro_roots = self.extra_ro_roots.clone();
        if let Some(dir) = bd_bin_dir() {
            let dir = std::fs::canonicalize(&dir).unwrap_or(dir);
            let base = env.get("PATH").cloned().unwrap_or_default();
            env.insert(
                "PATH".to_string(),
                format!("{}:{}", dir.to_string_lossy(), base),
            );
            extra_ro_roots.push(dir);
        }

        let config = build_config(OpenCodeConfigInit {
            profile_path: self.profile_path.clone(),
            attempt_dir,
            protected_dir: protected,
            sock_dir: sock,
            model: request.model.clone(),
            proxy_url: self.proxy_url.clone(),
            paid_proxy: self.paid_proxy,
            extra_ro_roots,
            extra_env: env,
        });

        let sandbox = SandboxSource::new(self.state_dir.clone());
        let sandbox_socket = sandbox.ensure()?;
        let harness_socket = self
            .state_dir
            .join(format!("harness-sweep-{}.sock", sanitize(&request.epic)));
        let mut harness = harness_process::open_opencode_harness(OpenCodeHarnessInit {
            transport: HarnessTransport::from_env(),
            config,
            root: root.clone(),
            model: request.model.clone(),
            generation: 1,
            sandbox_socket,
            harness_socket,
        })
        .map_err(ContractError::KnownFailure)?;

        let attempt = harness.start_attempt(StartAttemptRequest {
            workspace: request.repo.to_string_lossy().into_owned(),
            role: "designer".to_string(),
            genome_ref: ArtifactRef::derive("genome").to_string(),
            constraints_ref: ArtifactRef::derive("platform-constraints").to_string(),
            limits: Limits {
                wall_ms: self.turn_timeout.as_millis() as u64,
                tokens: 0,
                cost_micros: 0,
            },
            operation_id: OperationId::new(format!("op:sweep:{}", request.epic)),
        })?;

        let session = harness.create_session(CreateSessionRequest {
            role: "designer".to_string(),
            model: request.model.clone(),
            prompts_ref: request.prompt_path.to_string_lossy().into_owned(),
            handoff_refs: Vec::new(),
            tool_policy_ref: ArtifactRef::derive("tool-policy-designer").to_string(),
        })?;
        let op = harness.run_turn(
            &session,
            RunTurnRequest {
                input_ref: ArtifactRef::derive("designer-turn"),
                bounded_turns: 1,
                deadline_ms: self.turn_timeout.as_millis() as u64,
            },
        )?;
        let outcome = poll_terminal(&mut harness, &op, self.idle_timeout, self.turn_timeout);
        if matches!(outcome, PollOutcome::Stalled(_) | PollOutcome::TimedOut) {
            let _ = harness.cancel_operation(&op);
        }
        let _ = harness.close_session(&session);
        let _ = harness.stop_attempt(&attempt);

        match outcome {
            PollOutcome::Terminal(OperationState::Completed { .. }) => Ok(()),
            other => Err(ContractError::KnownFailure(format!(
                "designer session for {} ended without completion: {other:?}",
                request.epic
            ))),
        }
    }
}

/// Directory containing the `bd` binary (resolved from the conductor's PATH).
fn bd_bin_dir() -> Option<PathBuf> {
    std::env::var_os("PATH").and_then(|path| {
        std::env::split_paths(&path).find(|dir| dir.join("bd").is_file())
    })
}

/// Filesystem-safe name for a per-epic sweep directory.
fn sanitize(epic: &str) -> String {
    epic.chars()
        .map(|c| if c.is_ascii_alphanumeric() || c == '-' || c == '_' { c } else { '-' })
        .collect()
}

/// Create `path` (recursively) and return its canonical form.
fn canonical_dir(path: &Path) -> Result<PathBuf, ContractError> {
    std::fs::create_dir_all(path).map_err(|e| {
        ContractError::KnownFailure(format!("mkdir {}: {e}", path.display()))
    })?;
    std::fs::canonicalize(path)
        .map_err(|e| ContractError::KnownFailure(format!("resolve {}: {e}", path.display())))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn planner_gate_requires_opt_in_and_credential() {
        assert!(PlannerGate::new(true, true, None).available());
        assert!(!PlannerGate::new(false, true, None).available());
        assert!(!PlannerGate::new(true, false, None).available());
        // An explicit override unlocks the route regardless of paid gating.
        assert!(PlannerGate::new(false, false, Some("opencode/big-pickle")).available());
    }

    #[test]
    fn planner_gate_reason_is_never_a_silent_downgrade() {
        let off = PlannerGate::new(false, true, None);
        assert!(off.unavailable_reason().contains("never downgrading"));
        let no_cred = PlannerGate::new(true, false, None);
        assert!(no_cred.unavailable_reason().contains("never downgrading"));
    }

    #[test]
    fn fix_description_numbers_the_rule_named_violations_in_order() {
        let violations = vec![
            decomp::Violation {
                rule: "count-budget",
                detail: "2 children below minimum 8".to_string(),
            },
            decomp::Violation {
                rule: "schema-section",
                detail: "child x: headings missing".to_string(),
            },
        ];
        let text = render_fix_description("epic-1", &violations);
        assert!(text.contains("1. count-budget: 2 children below minimum 8"));
        assert!(text.contains("2. schema-section: child x: headings missing"));
    }

    #[test]
    fn sanitize_is_filesystem_safe() {
        assert_eq!(sanitize("agalma-i5h.4"), "agalma-i5h-4");
    }
}
