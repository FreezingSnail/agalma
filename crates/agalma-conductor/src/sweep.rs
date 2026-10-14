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
//! failed gate creates exactly one) reconciled with the durable
//! `decomp-attempts:<n>` epic label, so an attempt that never reached the gate
//! (an interrupted or empty decomposition) is still counted. An epic is a
//! candidate while the count is below [`MAX_DECOMP_RETRIES`]; the failure that
//! reaches the bound also adds the `human` label and stops the epic from being
//! re-swept.
//!
//! ## Timeout after commit
//!
//! A designer that hits the wall turn bound ([`DesignerOutcome::TimedOut`])
//! still persists the `bd` children and edges it committed before the bound. The
//! sweep reconciles that committed decomposition with the gate in this *same*
//! pass — never deferring to a later sweep (the work loop sweeps once per boot,
//! so deferral would strand the epic until restart) and never paying for a
//! second session for the same attempt. The timed-out attempt counts against the
//! retry budget exactly like any other (`decomp-attempts` / the `decomp-fix`
//! rework task), so a partial decomposition that cannot pass the gate is
//! retried and, at the bound, escalated by the existing retry policy.
//!
//! ## Durable per-epic planning outcome (recovery)
//!
//! Candidacy and retry counting read durable state on the epic, never only the
//! children's labels (which two failure paths — a designer that dies after
//! creating some children, and a completed session that produces zero children —
//! leave unlabelled, silently stranding the epic):
//!
//! - `decomp-attempts:<n>` — how many attempts the epic has consumed;
//! - `decomp-inflight` — an attempt is under way; present at the next sweep
//!   means it was interrupted, so the sweep finalizes the partial children
//!   ([`apply_gate`]) instead of starting a second designer session.
//!
//! The gate verdict stays on the `staged` children (`decomp-approved` /
//! `decomp-rejected`) and in the epic's comment, never as an epic label: bd
//! inherits parent labels onto new children, so an epic verdict label would
//! poison a later repaired decomposition with a stale rejection.
//!
//! `staged` children are decomposition membership; `decomp-fix` tasks are
//! bookkeeping/rework. Hand-authored (neither `staged` nor `decomp-fix`)
//! children mark an epic whose decomposition is not the sweep's to touch, so it
//! is never blindly replanned.
//!
//! ## Scoped single-epic entry (`agalma designer`, CP.6)
//!
//! [`sweep_epic`] runs exactly one designer attempt for exactly one existing
//! open epic, sharing the fleet sweep's per-epic semantics byte-for-byte (the
//! same recovery, stop gates, route resolution, attempt bookkeeping, session,
//! and CP.1 gate via [`run_epic_turn`]). It refuses epics the sweep would never
//! touch — a missing, closed, or human-escalated epic, or one that is not a
//! decomposition candidate (approved decomposition, hand-authored children, or
//! retries exhausted) — rather than silently replanning them, and it never
//! examines any other candidate epic. [`run_designer`] is the production entry
//! the `agalma designer` command calls; it mirrors [`run_sweep`]'s gating order
//! (stop check, then planner gate, then the live session).
//!
//! ## Testability
//!
//! [`sweep_epics`] takes a `&dyn DesignerSession`, so tests script the designer
//! (create a valid/invalid decomposition, or do nothing) without touching the
//! harness. [`LiveDesignerSession`] is the production implementation.

use std::path::{Path, PathBuf};
use std::rc::Rc;

use agalma_contracts::ids::ArtifactRef;
use agalma_contracts::{ContractError, OperationId, TaskFamily, TaskId, TaskQueueApi};
use agalma_taskqueue::{
    decomp, BdTaskQueue, EpicRecord, DECOMP_FIX_LABEL, HUMAN_LABEL, STAGED_LABEL,
};

use crate::routing;
use crate::session::{sanitize, LiveSession, SessionSpec};
use crate::stop::{bind_stop_control, StopControl};

/// Label applied to children a passing decomposition gate accepted.
///
/// Single-sourced from the task-queue adapter so the gate and the ready
/// predicate can never disagree on the approval bit.
pub const DECOMP_APPROVED: &str = agalma_taskqueue::DECOMP_APPROVED_LABEL;
/// Label applied to children a failing decomposition gate rejected.
pub const DECOMP_REJECTED: &str = agalma_taskqueue::DECOMP_REJECTED_LABEL;
/// Priority of the single rework task created on a failed gate (P1).
pub const DECOMP_FIX_PRIORITY: u8 = 1;
/// Defer date for the rework task: it is a designer/human artifact, never
/// ordinary fleet work, so it is hidden from `bd ready` by construction.
pub const DECOMP_FIX_DEFER: &str = "2099-01-01";
/// Number of failed decomposition gates tolerated before human escalation.
pub const MAX_DECOMP_RETRIES: usize = 3;
/// Durable in-flight marker label on the epic (see module docs).
pub const DECOMP_INFLIGHT: &str = agalma_taskqueue::DECOMP_INFLIGHT_LABEL;
/// Prefix of the durable per-epic attempt counter label (`decomp-attempts:<n>`).
pub const DECOMP_ATTEMPTS_PREFIX: &str = agalma_taskqueue::DECOMP_ATTEMPTS_PREFIX;
/// Prefix of the per-rework-task attempt label (`decomp-attempt:<n>`).
pub const DECOMP_ATTEMPT_PREFIX: &str = agalma_taskqueue::DECOMP_ATTEMPT_PREFIX;

/// The epic label recording that `n` attempts have been started.
fn attempts_label(n: usize) -> String {
    format!("{DECOMP_ATTEMPTS_PREFIX}{n}")
}

/// The rework-task label recording the attempt `n` it belongs to.
fn attempt_label(n: usize) -> String {
    format!("{DECOMP_ATTEMPT_PREFIX}{n}")
}

/// The highest attempt number recorded on the epic's labels (0 when absent).
fn max_attempt_label(labels: &[String]) -> usize {
    labels
        .iter()
        .filter_map(|label| label.strip_prefix(DECOMP_ATTEMPTS_PREFIX))
        .filter_map(|value| value.parse::<usize>().ok())
        .max()
        .unwrap_or(0)
}

/// Attempts an epic has consumed: the durable counter reconciled with the
/// number of `decomp-fix` rework tasks (the pre-existing attempt record).
fn observed_attempts(labels: &[String], fixes: usize) -> usize {
    max_attempt_label(labels).max(fixes)
}

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

/// Terminal outcome of one designer attempt.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum DesignerOutcome {
    /// The session reached the terminal `Completed` state.
    Completed,
    /// The session hit the wall turn bound before completing. Any `bd` children
    /// it committed before the bound persist and are reconciled by the gate in
    /// this same pass.
    TimedOut,
}

/// The designer-session seam: production runs the OpenCode adapter; tests script
/// it. The designer authors children via `bd` itself.
pub trait DesignerSession {
    /// Run one designer session for `request`, returning its terminal outcome.
    fn run(&self, request: &DesignerRequest) -> Result<DesignerOutcome, ContractError>;
}

/// Prior-decomposition feedback woven into a re-run designer prompt.
///
/// Gathered from the epic's backlog state when the sweep re-runs the designer
/// after a failed gate: the staged children the gate marked `decomp-rejected`,
/// the `decomp-fix` rework task(s), and the epic's own comments (rejection /
/// escalation reasons). Absent on a first decomposition.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct RetryContext {
    /// Ids of staged children the failed gate marked `decomp-rejected`.
    pub rejected_children: Vec<String>,
    /// Ids of the `decomp-fix` rework tasks under the epic.
    pub rework_tasks: Vec<String>,
    /// The epic's comment bodies.
    pub comments: Vec<String>,
}

impl RetryContext {
    /// Whether there is any feedback worth adding to the prompt.
    pub fn is_empty(&self) -> bool {
        self.rejected_children.is_empty()
            && self.rework_tasks.is_empty()
            && self.comments.is_empty()
    }
}

/// Compose the complete per-epic designer prompt: the canonical role text plus
/// the explicit input block naming the epic and its owned repository (and any
/// prior-decomposition feedback). This is the exact text handed to the harness
/// and posted to the vendor.
pub fn designer_prompt(epic: &str, repo: &Path, retry: Option<&RetryContext>) -> String {
    compose_designer_prompt(crate::DESIGNER_PROMPT, epic, repo, retry)
}

/// Compose a designer prompt from explicit canonical `role` text plus the
/// per-epic input block. Keeps the role instructions single-sourced (the caller
/// passes the canonical `designer.md` content) while making the selected epic,
/// its repository, and any retry feedback unambiguous.
pub fn compose_designer_prompt(
    role: &str,
    epic: &str,
    repo: &Path,
    retry: Option<&RetryContext>,
) -> String {
    let mut out = String::from(role.trim_end());
    out.push_str("\n\n");
    out.push_str(&render_input_block(epic, repo));
    if let Some(retry) = retry.filter(|ctx| !ctx.is_empty()) {
        out.push('\n');
        out.push_str(&render_retry_block(epic, retry));
    }
    out
}

/// The explicit per-epic input block: exact epic id, owning repository, and the
/// read-the-design instruction. No `<epic-id>` placeholder — the real id.
fn render_input_block(epic: &str, repo: &Path) -> String {
    format!(
        "## Input (this epic)\n\n\
         epic: {epic}\n\
         repository: {repo}\n\n\
         Read this epic's current design before decomposing:\n\n\
         ```\nbd show {epic} --json --include-comments\n```\n\n\
         Decompose only `{epic}`. Do not touch any other epic. Every child you \
         create must be parented to `{epic}` and reference its design for shared \
         context.\n",
        repo = repo.display()
    )
}

/// The failed-decomposition feedback block, rendered only when present.
fn render_retry_block(epic: &str, retry: &RetryContext) -> String {
    let mut out = format!(
        "## Prior decomposition feedback\n\n\
         A previous decomposition of `{epic}` failed validation. Read this \
         feedback and revise the children so `agalma plan-check --epic {epic}` \
         passes. Do not weaken the rules.\n"
    );
    if !retry.rejected_children.is_empty() {
        out.push_str("\nRejected staged child(ren):\n");
        for id in &retry.rejected_children {
            out.push_str(&format!("- {id}\n"));
        }
    }
    if !retry.rework_tasks.is_empty() {
        out.push_str("\nRework task(s):\n");
        for id in &retry.rework_tasks {
            out.push_str(&format!("- {id}\n"));
        }
    }
    if !retry.comments.is_empty() {
        out.push_str("\nEpic comments:\n");
        for comment in &retry.comments {
            out.push_str(&format!("- {comment}\n"));
        }
    }
    out
}

/// Gather prior-decomposition feedback for `epic`, or `None` on a clean first
/// run. Reads the epic's children (rejected + `decomp-fix`) and comments.
pub fn gather_retry_context(
    queue: &BdTaskQueue,
    epic: &str,
) -> Result<Option<RetryContext>, ContractError> {
    let children = queue.plan_children(epic)?;
    let rejected_children = children
        .iter()
        .filter(|child| child.labels.iter().any(|label| label == DECOMP_REJECTED))
        .map(|child| child.id.clone())
        .collect();
    let rework_tasks = children
        .iter()
        .filter(|child| child.labels.iter().any(|label| label == DECOMP_FIX_LABEL))
        .map(|child| child.id.clone())
        .collect();
    let comments = queue.projection(&TaskId::new(epic))?.comments;
    let context = RetryContext {
        rejected_children,
        rework_tasks,
        comments,
    };
    Ok(if context.is_empty() {
        None
    } else {
        Some(context)
    })
}

/// What the sweep did, for logging and tests.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct SweepReport {
    /// Set when the whole sweep was skipped (planner unavailable or a stop).
    pub skipped_reason: Option<String>,
    /// Set when a stop (STOP sentinel or kill latch) suppressed dispatch
    /// (`agalma-r2l`). Never cleared here.
    pub stopped: bool,
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
    /// Epics whose interrupted (in-flight) attempt was finalized on restart.
    pub recovered: Vec<String>,
    /// Epics whose designer attempt hit the turn bound; the committed
    /// decomposition was reconciled by the gate in this same pass.
    pub timed_out: Vec<String>,
}

/// Run one serial sweep over the open epics in `repo`.
///
/// Stop-safe entry point: honors the production stop policy (`<state>/STOP`
/// sentinel or the ledger kill latch) so an automatic boot sweep never launches
/// a paid planning session or edits the backlog while stopped (`agalma-r2l`).
/// Tests that script the control call [`sweep_epics_with_stop`].
///
/// Never spawns more than one designer session at a time (serial MVP, cap 1).
/// Returns a [`SweepReport`]; the caller decides whether to surface it.
pub fn sweep_epics(
    repo: &Path,
    state_dir: &Path,
    gate: &PlannerGate,
    designer: &dyn DesignerSession,
) -> Result<SweepReport, ContractError> {
    let stop = bind_stop_control(state_dir);
    sweep_epics_with_stop(repo, state_dir, gate, designer, &*stop)
}

/// Run one serial sweep honoring an explicit stop control.
///
/// The stop policy is applied at three points:
/// 1. before any dispatch (the whole sweep is suppressed while stopped);
/// 2. between candidate epics (top of each iteration, before any per-epic
///    mutation or paid dispatch);
/// 3. after each designer session returns (a stop that arrived mid-session
///    halts the sweep before the next epic and before any gate projection, so
///    the interrupted decomposition is preserved for reconciliation).
pub fn sweep_epics_with_stop(
    repo: &Path,
    state_dir: &Path,
    gate: &PlannerGate,
    designer: &dyn DesignerSession,
    stop: &dyn StopControl,
) -> Result<SweepReport, ContractError> {
    let mut report = SweepReport::default();
    if let Some(reason) = stop.decision().reason() {
        eprintln!("agalma: epic sweep skipped: {reason}");
        report.skipped_reason = Some(reason.to_string());
        report.stopped = true;
        return Ok(report);
    }
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
        // Stop gate between candidate epics: re-read before touching the epic
        // so no label is added and no session is dispatched once stopped.
        if let Some(reason) = stop.decision().reason() {
            eprintln!(
                "agalma: epic sweep: stop requested ({reason}); halting sweep before epic {}",
                epic.id
            );
            report.stopped = true;
            break;
        }
        if !is_candidate(&queue, &epic)? {
            continue;
        }
        report.epics_seen.push(epic.id.clone());
        match run_epic_turn(
            &mut queue,
            repo,
            state_dir,
            &prompt_path,
            &epic,
            gate,
            designer,
            stop,
            &mut report,
        )? {
            EpicTurn::Done => {}
            EpicTurn::Halted => {
                report.stopped = true;
                break;
            }
        }
    }
    Ok(report)
}

// ---------------------------------------------------------------------------
// Scoped single-epic entry (`agalma designer`, CP.6): one designer attempt for
// exactly one existing open epic, sharing the fleet sweep's per-epic semantics.
// ---------------------------------------------------------------------------

/// Run one scoped designer attempt for exactly one existing open epic.
///
/// Stop-safe entry point mirroring [`sweep_epics`]: honors the production stop
/// policy (`<state>/STOP` sentinel or the ledger kill latch) so the command
/// never launches a paid planning session or edits the backlog while stopped.
/// Tests that script the control call [`sweep_epic_with_stop`].
///
/// Only `epic` is examined; every other candidate epic is left untouched. The
/// named epic must be an open, decomposable candidate — a missing, closed, or
/// human-escalated epic, or one the sweep would never replan, is a hard error,
/// never a silent replan.
pub fn sweep_epic(
    repo: &Path,
    state_dir: &Path,
    epic: &str,
    gate: &PlannerGate,
    designer: &dyn DesignerSession,
) -> Result<SweepReport, ContractError> {
    let stop = bind_stop_control(state_dir);
    sweep_epic_with_stop(repo, state_dir, epic, gate, designer, &*stop)
}

/// Run one scoped designer attempt honoring an explicit stop control.
///
/// Semantics (the fleet sweep's per-epic path, plus a scoped lookup):
/// 1. an active stop skips with [`STOP_REASON`] and `stopped = true`;
/// 2. an unavailable planner route skips with
///    [`PlannerGate::unavailable_reason`];
/// 3. the epic is looked up among [`BdTaskQueue::open_epics`]; missing, closed,
///    or human-escalated is a [`ContractError::KnownFailure`];
/// 4. a non-candidate epic (approved decomposition, hand-authored children, or
///    retries exhausted) is a [`ContractError::KnownFailure`] — no candidacy
///    bypass, ever;
/// 5. otherwise the epic is pushed to `epics_seen` and one turn runs
///    ([`run_epic_turn`]); a mid-turn stop sets `stopped = true`.
pub fn sweep_epic_with_stop(
    repo: &Path,
    state_dir: &Path,
    epic: &str,
    gate: &PlannerGate,
    designer: &dyn DesignerSession,
    stop: &dyn StopControl,
) -> Result<SweepReport, ContractError> {
    let mut report = SweepReport::default();
    if let Some(reason) = stop.decision().reason() {
        eprintln!("agalma: designer skipped: {reason}");
        report.skipped_reason = Some(reason.to_string());
        report.stopped = true;
        return Ok(report);
    }
    if !gate.available() {
        let reason = gate.unavailable_reason();
        eprintln!("agalma: designer skipped: {reason}");
        report.skipped_reason = Some(reason);
        return Ok(report);
    }

    let prompt_path = write_designer_prompt(state_dir)?;
    let mut queue = BdTaskQueue::new(repo);
    let record = queue
        .open_epics()?
        .into_iter()
        .find(|candidate| candidate.id == epic)
        .ok_or_else(|| {
            ContractError::KnownFailure(format!(
                "epic {epic} is not an open epic in {} (missing, closed, or escalated to a human)",
                repo.display()
            ))
        })?;
    if !is_candidate(&queue, &record)? {
        return Err(ContractError::KnownFailure(format!(
            "epic {epic} is not a decomposition candidate (approved decomposition, hand-authored children, or retries exhausted)"
        )));
    }

    report.epics_seen.push(record.id.clone());
    match run_epic_turn(
        &mut queue,
        repo,
        state_dir,
        &prompt_path,
        &record,
        gate,
        designer,
        stop,
        &mut report,
    )? {
        EpicTurn::Done => {}
        EpicTurn::Halted => report.stopped = true,
    }
    Ok(report)
}

/// Terminal state of one per-epic designer turn, shared by the fleet sweep loop
/// and the scoped `agalma designer` entry point.
enum EpicTurn {
    /// The turn finished (dispatched and reconciled, or skipped); the caller
    /// continues with the next epic.
    Done,
    /// A stop arrived before dispatch or during the session; the caller halts.
    Halted,
}

/// Run one per-epic designer turn: interrupted-attempt recovery, the
/// pre-dispatch stop gate, planner-route resolution, attempt bookkeeping, the
/// designer session, the post-session stop gate, and same-pass gate
/// reconciliation (including timeout-after-commit).
///
/// Extracted from the fleet sweep loop so that loop and the scoped
/// [`sweep_epic_with_stop`] share the per-epic semantics byte-for-byte. `repo`
/// is threaded through explicitly (the target repository the designer session
/// authors children in) rather than recovered from `queue`.
#[allow(clippy::too_many_arguments)]
fn run_epic_turn(
    queue: &mut BdTaskQueue,
    repo: &Path,
    state_dir: &Path,
    prompt_path: &Path,
    epic: &EpicRecord,
    gate: &PlannerGate,
    designer: &dyn DesignerSession,
    stop: &dyn StopControl,
    report: &mut SweepReport,
) -> Result<EpicTurn, ContractError> {
    // An in-flight marker left by a previous sweep means that attempt was
    // interrupted (process death / cancelled session). Finalize the partial
    // children it left behind instead of starting a second designer session
    // over the same epic — never duplicate decomposition work.
    if epic.labels.iter().any(|label| label == DECOMP_INFLIGHT) {
        let fixes = count_fixes(queue, &epic.id)?;
        let attempt = observed_attempts(&epic.labels, fixes).max(1);
        eprintln!(
            "agalma: epic sweep: recovering interrupted decomposition for {} (attempt {attempt})",
            epic.id
        );
        apply_gate(queue, &epic.id, attempt, report)?;
        finish_attempt(queue, &epic.id)?;
        report.recovered.push(epic.id.clone());
        return Ok(EpicTurn::Done);
    }

    // Stop gate before paid dispatch: candidacy reads are done; re-check so a
    // stop that arrived during them suppresses the route/labels/session.
    if let Some(reason) = stop.decision().reason() {
        eprintln!(
            "agalma: epic sweep: stop requested ({reason}); halting before epic {}",
            epic.id
        );
        return Ok(EpicTurn::Halted);
    }

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
            return Ok(EpicTurn::Done);
        }
    };

    // Start (and durably record) this attempt before the session runs, so a
    // crash mid-session is recoverable and the retry bound survives restart.
    let fixes = count_fixes(queue, &epic.id)?;
    let attempt = observed_attempts(&epic.labels, fixes) + 1;
    queue.add_label(&TaskId::new(&epic.id), &attempts_label(attempt))?;
    queue.add_label(&TaskId::new(&epic.id), DECOMP_INFLIGHT)?;

    let request = DesignerRequest {
        epic: epic.id.clone(),
        repo: repo.to_path_buf(),
        state_dir: state_dir.to_path_buf(),
        model: route.model.clone(),
        prompt_path: prompt_path.to_path_buf(),
    };
    eprintln!(
        "agalma: epic sweep: running designer for {} (model {})",
        epic.id, route.model
    );
    match designer.run(&request) {
        Ok(DesignerOutcome::Completed) => report.epics_swept.push(epic.id.clone()),
        Ok(DesignerOutcome::TimedOut) => {
            eprintln!(
                "agalma: epic sweep: designer for {} hit the turn bound; reconciling committed decomposition",
                epic.id
            );
            report.timed_out.push(epic.id.clone());
        }
        Err(err) => {
            if stop.decision().is_denied() {
                eprintln!(
                    "agalma: epic sweep: designer session for {} cancelled by stop: {err}",
                    epic.id
                );
            } else {
                eprintln!(
                    "agalma: epic sweep: designer session for {} failed: {err}",
                    epic.id
                );
            }
        }
    }

    // Stop gate during running planning work: a stop that arrived while the
    // session ran halts the sweep. The durable `decomp-inflight` marker is
    // deliberately left in place (never implicitly cleared) so the next
    // post-resume sweep reconciles the interrupted decomposition instead of
    // starting a second session; no gate verdict is projected and no child
    // is touched. An unreadable established ledger (Unavailable) halts the same
    // way — uncertainty is fail-closed.
    if let Some(reason) = stop.decision().reason() {
        eprintln!(
            "agalma: epic sweep: stop requested ({reason}) during {}; preserving interrupted \
             decomposition for reconciliation",
            epic.id
        );
        return Ok(EpicTurn::Halted);
    }

    // Reconcile whatever the attempt left behind (a complete set, a partial
    // set, or nothing at all) and project one verdict.
    apply_gate(queue, &epic.id, attempt, report)?;
    finish_attempt(queue, &epic.id)?;
    Ok(EpicTurn::Done)
}

/// Single-line reason recorded when a stop suppresses a sweep. Kept as a public
/// alias of the shared [`crate::stop::STOPPED_REASON`] (`agalma-5fy.5`).
pub const STOP_REASON: &str = crate::stop::STOPPED_REASON;

/// Production entry point: build the planner gate and the live OpenCode designer
/// session, then sweep. Skips (with a log line) when a stop is active — before
/// constructing any session — or when the planner route is unavailable.
pub fn run_sweep(
    repo: &Path,
    state_dir: &Path,
    paid_available: bool,
    proxy_url: Option<String>,
    override_model: Option<&str>,
) -> Result<SweepReport, ContractError> {
    let stop = bind_stop_control(state_dir);
    run_sweep_with_stop(
        repo,
        state_dir,
        paid_available,
        proxy_url,
        override_model,
        stop,
    )
}

/// [`run_sweep`] honoring an explicit (already-bound) stop control.
///
/// The **same** control is used for admission and passed through the live
/// session seam, so the mid-turn poll reads the selected ledger binding rather
/// than opening a second database (`agalma-5fy.5`). The work loop already holds a
/// bound ledger handle and MUST prefer this entry with a
/// [`crate::stop::BoundStopControl`] built from that handle.
pub fn run_sweep_with_stop(
    repo: &Path,
    state_dir: &Path,
    paid_available: bool,
    proxy_url: Option<String>,
    override_model: Option<&str>,
    stop: Rc<dyn StopControl>,
) -> Result<SweepReport, ContractError> {
    if let Some(reason) = stop.decision().reason() {
        eprintln!("agalma: epic sweep skipped: {reason}");
        return Ok(SweepReport {
            skipped_reason: Some(reason.to_string()),
            stopped: true,
            ..SweepReport::default()
        });
    }
    let gate = PlannerGate::from_env(paid_available, override_model);
    if !gate.available() {
        let reason = gate.unavailable_reason();
        eprintln!("agalma: epic sweep skipped: {reason}");
        return Ok(SweepReport {
            skipped_reason: Some(reason),
            ..SweepReport::default()
        });
    }
    let session = LiveDesignerSession::build(
        state_dir,
        proxy_url,
        paid_available,
        Rc::clone(&stop),
    )?;
    sweep_epics_with_stop(repo, state_dir, &gate, &session, &*stop)
}

/// Production entry point for the scoped `agalma designer` command: build the
/// planner gate and the live OpenCode designer session, then run one scoped
/// attempt for `epic`. Mirrors [`run_sweep`]'s gating order — a stop is honored
/// (with a log line) before the gate is read and before any session is
/// constructed, and an unavailable planner route skips without downgrading.
pub fn run_designer(
    repo: &Path,
    state_dir: &Path,
    epic: &str,
    paid_available: bool,
    proxy_url: Option<String>,
    override_model: Option<&str>,
) -> Result<SweepReport, ContractError> {
    let stop = bind_stop_control(state_dir);
    run_designer_with_stop(
        repo,
        state_dir,
        epic,
        paid_available,
        proxy_url,
        override_model,
        stop,
    )
}

/// [`run_designer`] honoring an explicit (already-bound) stop control (see
/// [`run_sweep_with_stop`]).
pub fn run_designer_with_stop(
    repo: &Path,
    state_dir: &Path,
    epic: &str,
    paid_available: bool,
    proxy_url: Option<String>,
    override_model: Option<&str>,
    stop: Rc<dyn StopControl>,
) -> Result<SweepReport, ContractError> {
    if let Some(reason) = stop.decision().reason() {
        eprintln!("agalma: designer skipped: {reason}");
        return Ok(SweepReport {
            skipped_reason: Some(reason.to_string()),
            stopped: true,
            ..SweepReport::default()
        });
    }
    let gate = PlannerGate::from_env(paid_available, override_model);
    if !gate.available() {
        let reason = gate.unavailable_reason();
        eprintln!("agalma: designer skipped: {reason}");
        return Ok(SweepReport {
            skipped_reason: Some(reason),
            ..SweepReport::default()
        });
    }
    let session = LiveDesignerSession::build(
        state_dir,
        proxy_url,
        paid_available,
        Rc::clone(&stop),
    )?;
    sweep_epic_with_stop(repo, state_dir, epic, &gate, &session, &*stop)
}

/// Count an epic's `decomp-fix` rework tasks (its historical attempt record).
fn count_fixes(queue: &BdTaskQueue, epic: &str) -> Result<usize, ContractError> {
    Ok(queue
        .plan_children(epic)?
        .iter()
        .filter(|child| child.labels.iter().any(|l| l == DECOMP_FIX_LABEL))
        .count())
}

/// Clear the durable in-flight marker once an attempt's verdict is projected.
fn finish_attempt(queue: &mut BdTaskQueue, epic: &str) -> Result<(), ContractError> {
    queue.remove_label(&TaskId::new(epic), DECOMP_INFLIGHT)
}

/// Whether the epic is a decomposition candidate.
///
/// Candidacy is derived from durable state — the children's set membership and
/// the epic's own attempt/verdict labels — never from a single verdict label
/// alone, so interrupted, empty, and partially projected decompositions all
/// stay eligible across restart:
///
/// - an epic with hand-authored (neither `staged` nor `decomp-fix`) children is
///   never blindly replanned;
/// - a durable in-flight marker forces candidacy so the interrupted attempt is
///   finalized even if it already approved some children;
/// - an approved epic (every `staged` child carries `decomp-approved` with none
///   rejected) is never re-dispatched;
/// - otherwise the epic is a candidate while its attempt count is below
///   [`MAX_DECOMP_RETRIES`].
///
/// The verdict itself is durable on the children (`decomp-approved` /
/// `decomp-rejected`, the bits the ready predicate reads) and in the epic's
/// comments; it is deliberately never an epic label, which bd would inherit onto
/// any later child and thereby poison a repaired decomposition.
fn is_candidate(queue: &BdTaskQueue, epic: &EpicRecord) -> Result<bool, ContractError> {
    let children = queue.plan_children(&epic.id)?;
    let staged: Vec<&decomp::ChildTask> = children
        .iter()
        .filter(|child| child.labels.iter().any(|l| l == STAGED_LABEL))
        .collect();

    let has_hand_authored = children.iter().any(|child| {
        !child.labels.iter().any(|l| l == STAGED_LABEL)
            && !child.labels.iter().any(|l| l == DECOMP_FIX_LABEL)
    });
    if has_hand_authored {
        return Ok(false);
    }

    if epic.labels.iter().any(|label| label == DECOMP_INFLIGHT) {
        return Ok(true);
    }
    if !staged.is_empty() {
        let all_approved = staged
            .iter()
            .all(|child| child.labels.iter().any(|l| l == DECOMP_APPROVED));
        let any_rejected = staged
            .iter()
            .any(|child| child.labels.iter().any(|l| l == DECOMP_REJECTED));
        if all_approved && !any_rejected {
            return Ok(false);
        }
    }

    let fixes = children
        .iter()
        .filter(|child| child.labels.iter().any(|l| l == DECOMP_FIX_LABEL))
        .count();
    Ok(observed_attempts(&epic.labels, fixes) < MAX_DECOMP_RETRIES)
}

/// Run the CP.1 validator over the epic's `staged` children and project the gate
/// verdict onto the backlog.
///
/// Only `staged` children are decomposition membership; `decomp-fix` rework
/// tasks and any hand-authored children are never validated or labelled by the
/// gate. `attempt` is the durable attempt number this verdict belongs to: the
/// rework task carries it so a crash between rework creation and verdict
/// projection reuses the same task instead of fabricating a duplicate, and the
/// attempt reaching [`MAX_DECOMP_RETRIES`] escalates the epic to a human.
fn apply_gate(
    queue: &mut BdTaskQueue,
    epic: &str,
    attempt: usize,
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
            // The gate verdict supersedes any earlier one: a passing set must
            // not keep a stale `decomp-rejected` bit (the ready predicate hides
            // such a child forever, and a repaired decomposition would never
            // recover).
            queue.remove_label(&TaskId::new(&child.id), DECOMP_REJECTED)?;
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
        // Symmetrically, a failing set drops any stale approval so the verdict
        // is unambiguous (a partially projected approval must not veto retries).
        queue.remove_label(&TaskId::new(&child.id), DECOMP_APPROVED)?;
        queue.add_label(&TaskId::new(&child.id), DECOMP_REJECTED)?;
    }
    let description = render_fix_description(epic, &violations);
    let attempt_tag = attempt_label(attempt);
    // Reuse this attempt's rework task when a prior pass already created it (a
    // crash landed between rework creation and verdict projection), so an
    // interrupted attempt never fabricates a second rework task.
    let existing_fix = all.iter().find(|child| {
        child.labels.iter().any(|l| l == DECOMP_FIX_LABEL)
            && child.labels.iter().any(|l| l == &attempt_tag)
    });
    let fix_id = match existing_fix {
        Some(task) => task.id.clone(),
        None => {
            let labels = [DECOMP_FIX_LABEL, attempt_tag.as_str()];
            queue.create_task(
                &format!("decomp-fix: {epic}"),
                &description,
                Some(epic),
                DECOMP_FIX_PRIORITY,
                &labels,
                Some(DECOMP_FIX_DEFER),
            )?
        }
    };
    queue.comment(
        &TaskId::new(epic),
        &format!(
            "decomp-rejected: {} violation(s); rework task {fix_id}",
            violations.len()
        ),
    )?;
    report.rejected.push(epic.to_string());

    if attempt >= MAX_DECOMP_RETRIES {
        queue.add_label(&TaskId::new(epic), HUMAN_LABEL)?;
        queue.comment(
            &TaskId::new(epic),
            &format!(
                "decomp retries exhausted ({attempt}/{MAX_DECOMP_RETRIES}); escalating to a human"
            ),
        )?;
        eprintln!(
            "agalma: epic sweep: {epic} escalated to a human after {attempt} failed decomposition(s)"
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
    out.push_str("\nThe epic sweep re-runs the designer once the children are revised.\n");
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
        ContractError::KnownFailure(format!(
            "cannot write designer prompt {}: {e}",
            path.display()
        ))
    })?;
    Ok(path)
}

// ---------------------------------------------------------------------------
// Live designer session (OpenCode adapter)
// ---------------------------------------------------------------------------

/// Production [`DesignerSession`]: one confined OpenCode session per epic, run
/// in the target repo with the `designer.md` prompt and the planner-route model.
/// The session machinery is shared with the concierge through
/// [`crate::session`]; the designer ignores the captured output and relies on
/// the `bd` side effects it authors.
pub struct LiveDesignerSession {
    live: LiveSession,
}

impl LiveDesignerSession {
    /// Assemble the live session config (materializing the sandbox profile),
    /// using the same bound stop control passed for admission (`agalma-5fy.5`).
    pub fn build(
        state_dir: &Path,
        proxy_url: Option<String>,
        paid_available: bool,
        stop: Rc<dyn StopControl>,
    ) -> Result<Self, ContractError> {
        Ok(LiveDesignerSession {
            live: LiveSession::build(state_dir, proxy_url, paid_available, stop)?
                .with_turn_timeout(crate::composition::resolve_designer_timeout()),
        })
    }
}

impl DesignerSession for LiveDesignerSession {
    fn run(&self, request: &DesignerRequest) -> Result<DesignerOutcome, ContractError> {
        let key = sanitize(&request.epic);
        let prompt_path = write_live_designer_prompt(request)?;
        let spec = SessionSpec {
            role: "designer".to_string(),
            model: request.model.clone(),
            repo: request.repo.clone(),
            dir_key: format!("sweep/{key}"),
            socket_name: format!("harness-sweep-{key}.sock"),
            operation_id: OperationId::new(format!("op:sweep:{}", request.epic)),
            prompt_path,
            input_ref: ArtifactRef::derive("designer-turn"),
            tool_policy_ref: ArtifactRef::derive("tool-policy-designer"),
            genome_ref: ArtifactRef::derive("genome"),
            constraints_ref: ArtifactRef::derive("platform-constraints"),
            output_path: None,
            error_label: format!("designer session for {}", request.epic),
        };
        let end = crate::session::run_session_end(&self.live, &spec)?;
        Ok(match end.terminal {
            crate::session::SessionTerminal::Completed => DesignerOutcome::Completed,
            crate::session::SessionTerminal::TimedOut => DesignerOutcome::TimedOut,
        })
    }
}

/// Compose and materialize the per-epic designer prompt for `request`, returning
/// its canonical (absolute) path.
///
/// This is the production composition step: it reads the canonical role text
/// from `request.prompt_path`, gathers failed-decomposition feedback from the
/// target repo, composes the explicit input block (exact epic id + repository),
/// and writes the result. Handing the model the epic it must decompose — not an
/// opaque artifact id — is the whole point.
pub fn write_live_designer_prompt(request: &DesignerRequest) -> Result<PathBuf, ContractError> {
    let key = sanitize(&request.epic);
    let queue = BdTaskQueue::new(&request.repo);
    let retry = gather_retry_context(&queue, &request.epic)?;
    let role = std::fs::read_to_string(&request.prompt_path)
        .unwrap_or_else(|_| crate::DESIGNER_PROMPT.to_string());
    let text = compose_designer_prompt(&role, &request.epic, &request.repo, retry.as_ref());
    write_composed_prompt(&request.state_dir, &key, &text)
}

/// Write the composed per-epic designer prompt under the state dir and return
/// its canonical (absolute) path. Canonicalizing keeps it readable by the
/// harness whether the adapter runs in-process or behind the process boundary
/// (whose working directory differs).
fn write_composed_prompt(
    state_dir: &Path,
    key: &str,
    text: &str,
) -> Result<PathBuf, ContractError> {
    let dir = state_dir.join("sweep").join(key);
    std::fs::create_dir_all(&dir).map_err(|e| {
        ContractError::KnownFailure(format!("cannot create prompt dir {}: {e}", dir.display()))
    })?;
    let path = dir.join("prompt.md");
    std::fs::write(&path, text).map_err(|e| {
        ContractError::KnownFailure(format!(
            "cannot write designer prompt {}: {e}",
            path.display()
        ))
    })?;
    Ok(std::fs::canonicalize(&path).unwrap_or(path))
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
    fn designer_prompt_names_the_exact_epic_and_repo_for_two_requests() {
        // Two distinct crate/epic requests must each identify their own epic and
        // repository; neither may present a placeholder as the selected id.
        let a = designer_prompt("agalma-aaa", Path::new("/repos/alpha"), None);
        let b = designer_prompt("agalma-bbb", Path::new("/repos/beta"), None);

        assert!(a.contains("epic: agalma-aaa"), "{a}");
        assert!(a.contains("repository: /repos/alpha"), "{a}");
        assert!(!a.contains("agalma-bbb"), "request A leaked B's epic: {a}");

        assert!(b.contains("epic: agalma-bbb"), "{b}");
        assert!(b.contains("repository: /repos/beta"), "{b}");
        assert!(!b.contains("agalma-aaa"), "request B leaked A's epic: {b}");

        // The misleading literal placeholder is gone from the composed prompt.
        assert!(!a.contains("bd show <epic-id>"), "{a}");
        // The canonical role text is preserved verbatim.
        assert!(a.contains(crate::DESIGNER_PROMPT.trim_end()), "{a}");
    }

    #[test]
    fn designer_prompt_omits_retry_block_without_feedback() {
        let text = designer_prompt("agalma-aaa", Path::new("/repos/alpha"), None);
        assert!(!text.contains("Prior decomposition feedback"), "{text}");
    }

    #[test]
    fn designer_prompt_includes_failed_decomposition_feedback() {
        let retry = RetryContext {
            rejected_children: vec!["agalma-c1".to_string(), "agalma-c2".to_string()],
            rework_tasks: vec!["agalma-fix9".to_string()],
            comments: vec!["decomp-rejected: 2 violation(s)".to_string()],
        };
        let text = designer_prompt("agalma-aaa", Path::new("/repos/alpha"), Some(&retry));
        assert!(text.contains("Prior decomposition feedback"), "{text}");
        assert!(text.contains("agalma-c1"), "{text}");
        assert!(text.contains("agalma-c2"), "{text}");
        assert!(text.contains("agalma-fix9"), "{text}");
        assert!(text.contains("decomp-rejected: 2 violation(s)"), "{text}");
    }
}
