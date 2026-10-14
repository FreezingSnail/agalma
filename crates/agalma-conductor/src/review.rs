//! CP.5 per-epic review gate (post-merge drift check) — `agalma-i5h.6`.
//!
//! After the **last child of an epic lands** the ordinary work loop must not
//! treat the epic as done until a reviewer has compared the merged result with
//! the epic's own design/acceptance (design: `docs/planning-pipeline.md`
//! §"Review gate"). This module is the conductor-side port of maduin-review:
//!
//! 1. **start SHA** — the origin `HEAD` is recorded on the epic
//!    (`review-start:<sha>`) the first time one of its children lands
//!    ([`record_start_on_land`]); it is the baseline for the review diff.
//! 2. **review** — once every staged child is closed, one reviewer session runs
//!    over `git diff <start>..HEAD` vs the epic design/acceptance (the
//!    [`ReviewerSession`] seam; [`LiveReviewerSession`] is production).
//! 3. **verdict** — the session output carries **exactly one** marker line,
//!    [`MARKER_APPROVED`] or [`MARKER_DRIFT`]. [`parse_verdict`] is strict:
//!    zero markers, more than one, or an unrecognized marker is an error — the
//!    gate never silently passes.
//! 4. **consequence** — `APPROVED` labels the epic `review-approved` and clears
//!    any hold. `DRIFT <feedback>` files ONE P1 `drift-fix` task under the epic
//!    (idempotent per attempt) and applies the `review-drift` **hold**:
//!    [`dispatch_hold`] then reports the epic, and the work loop withholds
//!    ordinary dispatch until the drift is cleared (by a later `APPROVED`
//!    review). A marker error holds the same way (fail closed) without filing a
//!    rework task.
//!
//! ## Durable per-epic review state (recovery)
//!
//! All review state lives on the epic so it survives restart, mirroring the
//! CP.3 decomposition gate:
//!
//! - `review-start:<sha>` — the diff baseline (first child land);
//! - `review-attempts:<n>` — how many review attempts the epic has consumed,
//!   reconciled with the number of `drift-fix` tasks;
//! - `review-drift` — a drift hold is active (ordinary dispatch withheld);
//! - `review-approved` — the epic passed review (terminal, never re-reviewed);
//! - `human` — retries exhausted; the epic is handed to a human.
//!
//! The verdict itself rides the `drift-fix` task(s) and the epic's comments,
//! never a bare epic verdict label the way [`REVIEW_DRIFT_LABEL`] is used (that
//! one *is* an epic label because it must gate dispatch — it is removed the
//! moment an approval lands).
//!
//! ## Testability
//!
//! [`review_epics`] takes a `&dyn ReviewerSession`, so tests script the
//! reviewer (approve, drift, or emit no marker) without touching the harness.

use std::path::{Path, PathBuf};
use std::process::Command;
use std::rc::Rc;

use agalma_contracts::ids::ArtifactRef;
use agalma_contracts::{ContractError, OperationId, TaskFamily, TaskId, TaskQueueApi};
use agalma_taskqueue::{BdTaskQueue, EpicRecord, HUMAN_LABEL, STAGED_LABEL};
use serde::Deserialize;
use serde_json::Value;

use crate::routing;
use crate::session::{sanitize, LiveSession, SessionSpec};
use crate::stop::{bind_stop_control, StopControl};
use crate::sweep::PlannerGate;

/// Label of the P1 rework task filed on a drift verdict (design: `drift-fix`).
pub const DRIFT_FIX_LABEL: &str = "drift-fix";
/// Epic label marking an unresolved review drift (the dispatch hold).
pub const REVIEW_DRIFT_LABEL: &str = "review-drift";
/// Epic label marking a passing review (terminal).
pub const REVIEW_APPROVED_LABEL: &str = "review-approved";
/// Prefix of the epic diff-baseline label (`review-start:<sha>`).
pub const REVIEW_START_PREFIX: &str = "review-start:";
/// Prefix of the durable per-epic attempt counter (`review-attempts:<n>`).
pub const REVIEW_ATTEMPTS_PREFIX: &str = "review-attempts:";
/// Prefix of the per-rework-task attempt label (`review-attempt:<n>`).
pub const REVIEW_ATTEMPT_PREFIX: &str = "review-attempt:";
/// Priority of the drift-fix rework task (P1).
pub const DRIFT_FIX_PRIORITY: u8 = 1;
/// Defer date for the drift-fix task: it is a rework/human artifact, never
/// ordinary fleet work, so it is hidden from `bd ready` by construction.
pub const DRIFT_FIX_DEFER: &str = "2099-01-01";
/// Failed reviews tolerated before human escalation.
pub const MAX_REVIEW_RETRIES: usize = 3;

/// The marker-line prefix every reviewer verdict starts with.
pub const MARKER_PREFIX: &str = "REVIEW:";
/// The exact approval marker line.
pub const MARKER_APPROVED: &str = "REVIEW: APPROVED";
/// The drift marker line prefix (feedback follows).
pub const MARKER_DRIFT: &str = "REVIEW: DRIFT";

/// A parsed reviewer verdict.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum ReviewVerdict {
    /// The merged diff matches the epic design/acceptance in full.
    Approved,
    /// The merged diff drifts; the reviewer supplied concrete feedback.
    Drift { feedback: String },
}

/// Parse **exactly one** reviewer marker line from `output`.
///
/// Strict by design (never a silent pass): zero markers, more than one marker,
/// a `DRIFT` marker with no feedback, or an unrecognized marker are all errors.
pub fn parse_verdict(output: &str) -> Result<ReviewVerdict, ContractError> {
    let markers: Vec<&str> = output
        .lines()
        .map(str::trim)
        .filter(|line| line.starts_with(MARKER_PREFIX))
        .collect();
    match markers.as_slice() {
        [] => Err(ContractError::KnownFailure(
            "reviewer output carries no `REVIEW:` marker line (never a silent pass)".to_string(),
        )),
        [line] => parse_marker(line),
        many => Err(ContractError::KnownFailure(format!(
            "reviewer output carries {} `REVIEW:` marker lines; exactly one is required",
            many.len()
        ))),
    }
}

/// Parse a single marker line into a verdict.
fn parse_marker(line: &str) -> Result<ReviewVerdict, ContractError> {
    if line == MARKER_APPROVED {
        return Ok(ReviewVerdict::Approved);
    }
    if let Some(rest) = line.strip_prefix(MARKER_DRIFT) {
        let feedback = rest.trim();
        if feedback.is_empty() {
            return Err(ContractError::KnownFailure(
                "`REVIEW: DRIFT` marker carries no feedback".to_string(),
            ));
        }
        return Ok(ReviewVerdict::Drift {
            feedback: feedback.to_string(),
        });
    }
    Err(ContractError::KnownFailure(format!(
        "unrecognized review marker line: {line:?}"
    )))
}

/// Inputs handed to one reviewer session.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ReviewRequest {
    /// Epic under review.
    pub epic: String,
    /// Target repository (the bd workspace + git checkout).
    pub repo: PathBuf,
    /// Conductor state dir (session scratch).
    pub state_dir: PathBuf,
    /// Planner-route model id pinned for the session.
    pub model: String,
    /// The composed per-epic prompt file the session reads.
    pub prompt_path: PathBuf,
    /// The diff baseline commit (`review-start:<sha>` on the epic).
    pub start_sha: String,
    /// The integrated head commit reviewed.
    pub head_sha: String,
    /// The merged diff file the prompt references.
    pub diff_path: PathBuf,
}

/// The reviewer-session seam: production runs the OpenCode adapter; tests
/// script it. On success it returns the session output contract (the marker).
pub trait ReviewerSession {
    /// Run one reviewer session for `request`, returning its output contract.
    fn run(&self, request: &ReviewRequest) -> Result<String, ContractError>;
}

/// What the review gate did, for logging and tests.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct ReviewReport {
    /// Set when the whole gate was skipped (planner unavailable or a stop).
    pub skipped_reason: Option<String>,
    /// Set when a stop suppressed the gate.
    pub stopped: bool,
    /// Epic candidates the gate examined.
    pub epics_seen: Vec<String>,
    /// Epics the reviewer approved.
    pub approved: Vec<String>,
    /// Epics the reviewer flagged as drifted.
    pub drift: Vec<String>,
    /// Epics escalated to a human (retries exhausted).
    pub escalated: Vec<String>,
    /// Epics whose reviewer output carried no valid marker (fail closed).
    pub errored: Vec<String>,
    /// Epics left under an active review hold (drift or error).
    pub held: Vec<String>,
}

/// The epic label recording attempt `n`.
fn attempts_label(n: usize) -> String {
    format!("{REVIEW_ATTEMPTS_PREFIX}{n}")
}

/// The rework-task label recording the attempt `n` it belongs to.
fn attempt_label(n: usize) -> String {
    format!("{REVIEW_ATTEMPT_PREFIX}{n}")
}

/// The highest review-attempt number recorded on the epic's labels (0 absent).
fn max_attempt_label(labels: &[String]) -> usize {
    labels
        .iter()
        .filter_map(|label| label.strip_prefix(REVIEW_ATTEMPTS_PREFIX))
        .filter_map(|value| value.parse::<usize>().ok())
        .max()
        .unwrap_or(0)
}

/// Attempts an epic has consumed: durable counter reconciled with the number of
/// `drift-fix` rework tasks (the pre-existing attempt record).
fn observed_attempts(labels: &[String], fixes: usize) -> usize {
    max_attempt_label(labels).max(fixes)
}

/// The diff baseline recorded on the epic, when present.
pub fn review_start_sha(labels: &[String]) -> Option<String> {
    labels
        .iter()
        .filter_map(|label| label.strip_prefix(REVIEW_START_PREFIX))
        .find(|sha| !sha.is_empty())
        .map(str::to_string)
}

/// Record the epic's review baseline (`review-start:<HEAD>`) the first time one
/// of its children lands: a no-op once a baseline exists, and a no-op for a task
/// with no parent epic.
pub fn record_start_on_land(repo: &Path, task_id: &str) -> Result<(), ContractError> {
    let Some(epic) = bd_parent(repo, task_id)? else {
        return Ok(());
    };
    let mut queue = BdTaskQueue::new(repo);
    let labels = queue.projection(&TaskId::new(&epic))?.labels;
    if review_start_sha(&labels).is_some() {
        return Ok(());
    }
    if let Some(head) = head_sha(repo)? {
        queue.add_label(&TaskId::new(&epic), &format!("{REVIEW_START_PREFIX}{head}"))?;
    }
    Ok(())
}

/// The epic currently holding ordinary dispatch: an open epic under an
/// unresolved review drift (the `review-drift` label) that has not been handed
/// to a human. `None` when no hold is active.
pub fn dispatch_hold(queue: &BdTaskQueue) -> Result<Option<String>, ContractError> {
    for epic in queue.open_epics()? {
        let held = epic.labels.iter().any(|l| l == REVIEW_DRIFT_LABEL);
        let escalated = epic.labels.iter().any(|l| l == HUMAN_LABEL);
        if held && !escalated {
            return Ok(Some(epic.id));
        }
    }
    Ok(None)
}

/// Run one serial review pass over `repo`.
///
/// Stop-safe entry point (production stop policy). Never spawns more than one
/// reviewer session at a time (serial MVP, cap 1). Returns a [`ReviewReport`].
pub fn review_epics(
    repo: &Path,
    state_dir: &Path,
    gate: &PlannerGate,
    reviewer: &dyn ReviewerSession,
) -> Result<ReviewReport, ContractError> {
    let stop = bind_stop_control(state_dir);
    review_epics_with_stop(repo, state_dir, gate, reviewer, &*stop)
}

/// Run one serial review pass honoring an explicit stop control.
///
/// The stop policy is checked before any dispatch and between candidate epics,
/// exactly like the CP.3 sweep: a stop suppresses the paid reviewer session and
/// the backlog mutation that follows it.
pub fn review_epics_with_stop(
    repo: &Path,
    state_dir: &Path,
    gate: &PlannerGate,
    reviewer: &dyn ReviewerSession,
    stop: &dyn StopControl,
) -> Result<ReviewReport, ContractError> {
    let mut report = ReviewReport::default();
    if let Some(reason) = stop.decision().reason() {
        eprintln!("agalma: review gate skipped: {reason}");
        report.skipped_reason = Some(reason.to_string());
        report.stopped = true;
        return Ok(report);
    }
    if !gate.available() {
        let reason = gate.unavailable_reason();
        eprintln!("agalma: review gate skipped: {reason}");
        report.skipped_reason = Some(reason);
        return Ok(report);
    }

    let mut queue = BdTaskQueue::new(repo);
    let epics = queue.open_epics()?;
    for epic in epics {
        if let Some(reason) = stop.decision().reason() {
            eprintln!(
                "agalma: review gate: stop requested ({reason}); halting before epic {}",
                epic.id
            );
            report.stopped = true;
            break;
        }
        let children = read_children(repo, &epic.id)?;
        if !is_review_candidate(&epic, &children) {
            continue;
        }
        report.epics_seen.push(epic.id.clone());

        // Stop gate after candidate reads and before any queue/route effect
        // (`agalma-5fy.5`): a stop that arrived during the reads suppresses the
        // baseline hold, the route decision, the attempt label and the session.
        if let Some(reason) = stop.decision().reason() {
            eprintln!(
                "agalma: review gate: stop requested ({reason}); halting before review of {}",
                epic.id
            );
            report.stopped = true;
            break;
        }

        // A review without a recorded baseline cannot diff against the design;
        // fail safe (hold) rather than approve on an empty diff.
        let Some(start) = review_start_sha(&epic.labels) else {
            eprintln!(
                "agalma: review gate: epic {} has no review-start baseline; holding",
                epic.id
            );
            let attempt = next_attempt(&queue, &epic.id)?;
            apply_error(
                &mut queue,
                &epic.id,
                attempt,
                "no review-start baseline recorded",
                &mut report,
            )?;
            continue;
        };

        // Resolve (and durably record) the strongest route for the reviewer
        // under a `review:<epic>` scope. Fails closed when the planner route is
        // unavailable — never a silent downgrade.
        let route = match routing::resolve_route(
            state_dir,
            &format!("review:{}", epic.id),
            "planner",
            TaskFamily::Feature,
            gate.allow_paid,
            gate.paid_available,
            gate.override_model.as_deref(),
        ) {
            Ok(route) => route,
            Err(err) => {
                eprintln!(
                    "agalma: review gate: review route unavailable for {}: {err}",
                    epic.id
                );
                continue;
            }
        };

        let Some(head) = head_sha(repo)? else {
            eprintln!(
                "agalma: review gate: epic {} has no HEAD commit to review",
                epic.id
            );
            continue;
        };
        let diff = git_diff(repo, &start, &head)?;
        let prompt_path = write_review_prompt(state_dir, repo, &epic.id, &start, &head, &diff)?;
        let key = sanitize(&epic.id);
        let diff_path = state_dir.join("review").join(&key).join("diff.patch");
        let request = ReviewRequest {
            epic: epic.id.clone(),
            repo: repo.to_path_buf(),
            state_dir: state_dir.to_path_buf(),
            model: route.model.clone(),
            prompt_path,
            start_sha: start,
            head_sha: head,
            diff_path,
        };

        // Stop gate immediately before the paid dispatch and the attempt-label
        // mutation.
        if let Some(reason) = stop.decision().reason() {
            eprintln!(
                "agalma: review gate: stop requested ({reason}); halting before dispatching the \
                 reviewer for {}",
                epic.id
            );
            report.stopped = true;
            break;
        }
        eprintln!(
            "agalma: review gate: reviewing {} (model {})",
            epic.id, route.model
        );
        let attempt = next_attempt(&queue, &epic.id)?;
        queue.add_label(&TaskId::new(&epic.id), &attempts_label(attempt))?;
        match reviewer.run(&request) {
            Ok(output) => {
                // Stop gate after the session returns, before any verdict/error
                // projection: a stop that arrived mid-session (or an established
                // ledger that became unreadable) suppresses the projection so no
                // epic is closed and no drift task is created. The attempt label
                // already recorded is preserved (never implicitly cleared).
                if let Some(reason) = stop.decision().reason() {
                    eprintln!(
                        "agalma: review gate: stop requested ({reason}) after the reviewer session \
                         for {}; preserving evidence without projecting a verdict",
                        epic.id
                    );
                    report.stopped = true;
                    break;
                }
                match parse_verdict(&output) {
                    Ok(verdict) => apply_verdict(
                        &mut queue,
                        &epic.id,
                        attempt,
                        &request.start_sha,
                        verdict,
                        &mut report,
                    )?,
                    Err(err) => {
                        apply_error(&mut queue, &epic.id, attempt, &err.to_string(), &mut report)?
                    }
                }
            }
            Err(err) => {
                if stop.decision().is_denied() {
                    eprintln!(
                        "agalma: review gate: reviewer session for {} cancelled by stop: {err}",
                        epic.id
                    );
                    report.stopped = true;
                    break;
                }
                apply_error(&mut queue, &epic.id, attempt, &err.to_string(), &mut report)?;
            }
        }
    }
    Ok(report)
}

/// Whether the epic is a review candidate: at least one `staged` child, every
/// staged child closed (the decomposition fully landed), not yet approved, and
/// not escalated to a human. Rework (`drift-fix`) children are bookkeeping and
/// never gate completion.
fn is_review_candidate(epic: &EpicRecord, children: &[ChildStatus]) -> bool {
    let staged: Vec<&ChildStatus> = children
        .iter()
        .filter(|child| child.labels.iter().any(|l| l == STAGED_LABEL))
        .collect();
    if staged.is_empty() {
        return false;
    }
    if !staged.iter().all(|child| child.status == "closed") {
        return false;
    }
    if epic
        .labels
        .iter()
        .any(|l| l == REVIEW_APPROVED_LABEL || l == HUMAN_LABEL)
    {
        return false;
    }
    true
}

/// The next attempt number for `epic` (prior `drift-fix` tasks reconciled with
/// the durable counter).
fn next_attempt(queue: &BdTaskQueue, epic: &str) -> Result<usize, ContractError> {
    let fixes = count_drift_fixes(queue, epic)?;
    let labels = queue.projection(&TaskId::new(epic))?.labels;
    Ok(observed_attempts(&labels, fixes) + 1)
}

/// Count an epic's `drift-fix` rework tasks (its historical attempt record).
fn count_drift_fixes(queue: &BdTaskQueue, epic: &str) -> Result<usize, ContractError> {
    Ok(queue
        .plan_children(epic)?
        .iter()
        .filter(|child| child.labels.iter().any(|l| l == DRIFT_FIX_LABEL))
        .count())
}

/// Project an `APPROVED` verdict: label the epic approved and clear the hold.
fn apply_verdict(
    queue: &mut BdTaskQueue,
    epic: &str,
    attempt: usize,
    start: &str,
    verdict: ReviewVerdict,
    report: &mut ReviewReport,
) -> Result<(), ContractError> {
    match verdict {
        ReviewVerdict::Approved => {
            queue.remove_label(&TaskId::new(epic), REVIEW_DRIFT_LABEL)?;
            queue.add_label(&TaskId::new(epic), REVIEW_APPROVED_LABEL)?;
            queue.comment(
                &TaskId::new(epic),
                &format!("review-approved: diff since {start} matches the epic design/acceptance"),
            )?;
            report.approved.push(epic.to_string());
        }
        ReviewVerdict::Drift { feedback } => {
            queue.add_label(&TaskId::new(epic), REVIEW_DRIFT_LABEL)?;
            let description = render_drift_description(epic, start, &feedback);
            let tag = attempt_label(attempt);
            // Reuse this attempt's rework task when a prior pass already created
            // it (a crash landed between rework creation and verdict
            // projection), so an interrupted attempt never fabricates a second.
            let existing = queue.plan_children(epic)?.into_iter().find(|child| {
                child.labels.iter().any(|l| l == DRIFT_FIX_LABEL)
                    && child.labels.iter().any(|l| l == &tag)
            });
            let fix_id = match existing {
                Some(task) => task.id,
                None => {
                    let labels = [DRIFT_FIX_LABEL, tag.as_str()];
                    queue.create_task(
                        &format!("drift-fix: {epic}"),
                        &description,
                        Some(epic),
                        DRIFT_FIX_PRIORITY,
                        &labels,
                        Some(DRIFT_FIX_DEFER),
                    )?
                }
            };
            queue.comment(
                &TaskId::new(epic),
                &format!("review-drift: rework task {fix_id}"),
            )?;
            report.drift.push(epic.to_string());
            report.held.push(epic.to_string());
            escalate_if_exhausted(queue, epic, attempt, report)?;
        }
    }
    Ok(())
}

/// Project a marker/parse error: hold dispatch (fail closed), never approve,
/// and escalate once the retry budget is exhausted.
fn apply_error(
    queue: &mut BdTaskQueue,
    epic: &str,
    attempt: usize,
    reason: &str,
    report: &mut ReviewReport,
) -> Result<(), ContractError> {
    queue.add_label(&TaskId::new(epic), REVIEW_DRIFT_LABEL)?;
    queue.comment(
        &TaskId::new(epic),
        &format!("review-error: {reason} (holding; never a silent pass)"),
    )?;
    report.errored.push(epic.to_string());
    report.held.push(epic.to_string());
    escalate_if_exhausted(queue, epic, attempt, report)?;
    Ok(())
}

/// Escalate the epic to a human once the review retry budget is exhausted.
fn escalate_if_exhausted(
    queue: &mut BdTaskQueue,
    epic: &str,
    attempt: usize,
    report: &mut ReviewReport,
) -> Result<(), ContractError> {
    if attempt < MAX_REVIEW_RETRIES {
        return Ok(());
    }
    queue.add_label(&TaskId::new(epic), HUMAN_LABEL)?;
    queue.comment(
        &TaskId::new(epic),
        &format!(
            "review retries exhausted ({attempt}/{MAX_REVIEW_RETRIES}); escalating to a human"
        ),
    )?;
    eprintln!("agalma: review gate: {epic} escalated to a human after {attempt} failed review(s)");
    report.escalated.push(epic.to_string());
    Ok(())
}

/// The drift-fix task body: the reviewer feedback plus the review baseline.
fn render_drift_description(epic: &str, start: &str, feedback: &str) -> String {
    format!(
        "Review drift for epic {epic}.\n\n\
         The post-merge review of `git diff {start}..HEAD` found the merged result \
         does not match the epic design/acceptance. Fix the drift, then the epic's \
         review re-runs.\n\n\
         Reviewer feedback:\n{feedback}\n\n\
         The review gate holds ordinary dispatch until this drift is cleared.\n"
    )
}

/// Compose the complete per-epic reviewer prompt from the canonical role text
/// plus the explicit input block (exact epic id, repository, diff path).
pub fn reviewer_prompt(
    epic: &str,
    repo: &Path,
    start_sha: &str,
    head_sha: &str,
    diff_path: &Path,
) -> String {
    compose_reviewer_prompt(
        crate::REVIEWER_PROMPT,
        epic,
        repo,
        start_sha,
        head_sha,
        diff_path,
    )
}

/// Compose a reviewer prompt from explicit canonical `role` text plus the
/// per-epic input block.
pub fn compose_reviewer_prompt(
    role: &str,
    epic: &str,
    repo: &Path,
    start_sha: &str,
    head_sha: &str,
    diff_path: &Path,
) -> String {
    let mut out = String::from(role.trim_end());
    out.push_str("\n\n## Input (this epic)\n\n");
    out.push_str(&format!("epic: {epic}\n"));
    out.push_str(&format!("repository: {repo}\n", repo = repo.display()));
    out.push_str(&format!("diff baseline: {start_sha}\n"));
    out.push_str(&format!("reviewed head: {head_sha}\n"));
    out.push_str(&format!(
        "merged diff: {diff}\n\n",
        diff = diff_path.display()
    ));
    out.push_str(&format!(
        "Read this epic's design/acceptance before deciding:\n\n\
         ```\nbd show {epic} --json --include-comments\n```\n\n\
         Review only `{epic}`. Emit exactly one `REVIEW:` marker line.\n"
    ));
    out
}

/// Write the composed per-epic reviewer prompt plus the merged diff under the
/// state dir and return the prompt path. Canonicalizing keeps them readable by
/// the harness whether it runs in-process or behind the process boundary.
fn write_review_prompt(
    state_dir: &Path,
    repo: &Path,
    epic: &str,
    start_sha: &str,
    head_sha: &str,
    diff: &str,
) -> Result<PathBuf, ContractError> {
    let key = sanitize(epic);
    let dir = state_dir.join("review").join(&key);
    std::fs::create_dir_all(&dir).map_err(|e| {
        ContractError::KnownFailure(format!("cannot create review dir {}: {e}", dir.display()))
    })?;
    let diff_path = dir.join("diff.patch");
    std::fs::write(&diff_path, diff).map_err(|e| {
        ContractError::KnownFailure(format!(
            "cannot write review diff {}: {e}",
            diff_path.display()
        ))
    })?;
    let prompt_path = dir.join("prompt.md");
    let text = reviewer_prompt(epic, repo, start_sha, head_sha, &diff_path);
    std::fs::write(&prompt_path, text).map_err(|e| {
        ContractError::KnownFailure(format!(
            "cannot write review prompt {}: {e}",
            prompt_path.display()
        ))
    })?;
    Ok(std::fs::canonicalize(&prompt_path).unwrap_or(prompt_path))
}

// ---------------------------------------------------------------------------
// git / bd reads
// ---------------------------------------------------------------------------

/// `git rev-parse HEAD` in `repo`; `None` when the repo has no commits yet.
fn head_sha(repo: &Path) -> Result<Option<String>, ContractError> {
    let out = Command::new("git")
        .arg("-C")
        .arg(repo)
        .args(["rev-parse", "HEAD"])
        .env("GIT_TERMINAL_PROMPT", "0")
        .output()
        .map_err(|e| ContractError::KnownFailure(format!("git rev-parse HEAD: {e}")))?;
    if !out.status.success() {
        return Ok(None);
    }
    let sha = String::from_utf8_lossy(&out.stdout).trim().to_string();
    Ok(if sha.is_empty() { None } else { Some(sha) })
}

/// `git diff <start>..<head>` in `repo`.
fn git_diff(repo: &Path, start: &str, head: &str) -> Result<String, ContractError> {
    let out = Command::new("git")
        .arg("-C")
        .arg(repo)
        .args(["diff", &format!("{start}..{head}")])
        .env("GIT_TERMINAL_PROMPT", "0")
        .output()
        .map_err(|e| ContractError::KnownFailure(format!("git diff: {e}")))?;
    if !out.status.success() {
        return Err(ContractError::KnownFailure(format!(
            "git diff {start}..{head} failed: {}",
            String::from_utf8_lossy(&out.stderr).trim()
        )));
    }
    Ok(String::from_utf8_lossy(&out.stdout).into_owned())
}

/// The direct parent of a task, or `None` (root task / not found). Read via the
/// `bd` CLI, mirroring [`crate::concierge`].
fn bd_parent(repo: &Path, task_id: &str) -> Result<Option<String>, ContractError> {
    let out = Command::new("bd")
        .arg("--db")
        .arg(repo.join(".beads"))
        .args(["show", task_id, "--json"])
        .current_dir(repo)
        .env("BD_NON_INTERACTIVE", "1")
        .env("GIT_TERMINAL_PROMPT", "0")
        .output()
        .map_err(|e| ContractError::KnownFailure(format!("bd show {task_id}: {e}")))?;
    if !out.status.success() {
        return Ok(None);
    }
    let value: Value = serde_json::from_slice(&out.stdout).map_err(|e| {
        ContractError::KnownFailure(format!("bd show {task_id}: invalid JSON: {e}"))
    })?;
    let issue = value
        .as_array()
        .and_then(|issues| issues.first())
        .cloned()
        .unwrap_or(value);
    Ok(issue
        .get("parent")
        .and_then(Value::as_str)
        .map(str::to_string))
}

/// One child as the review gate sees it (status + labels).
#[derive(Clone, Debug, Deserialize)]
struct ChildStatus {
    status: String,
    #[serde(default)]
    labels: Vec<String>,
}

/// Read an epic's children (including deferred/staged ones) with their status
/// via the `bd` CLI: `plan_children` drops status, which the gate needs.
fn read_children(repo: &Path, epic: &str) -> Result<Vec<ChildStatus>, ContractError> {
    let out = Command::new("bd")
        .arg("--db")
        .arg(repo.join(".beads"))
        .args(["list", "--parent", epic, "--all", "--json", "--limit", "0"])
        .current_dir(repo)
        .env("BD_NON_INTERACTIVE", "1")
        .env("GIT_TERMINAL_PROMPT", "0")
        .output()
        .map_err(|e| ContractError::KnownFailure(format!("bd list --parent {epic}: {e}")))?;
    if !out.status.success() {
        return Err(ContractError::KnownFailure(format!(
            "bd list --parent {epic} failed: {}",
            String::from_utf8_lossy(&out.stderr).trim()
        )));
    }
    let text = String::from_utf8_lossy(&out.stdout);
    let trimmed = text.trim();
    if trimmed.is_empty() {
        return Ok(Vec::new());
    }
    let children: Vec<ChildStatus> = serde_json::from_str(trimmed).map_err(|e| {
        ContractError::KnownFailure(format!("bd list --parent {epic}: invalid JSON: {e}"))
    })?;
    Ok(children)
}

// ---------------------------------------------------------------------------
// Live reviewer session (OpenCode adapter)
// ---------------------------------------------------------------------------

/// Production [`ReviewerSession`]: one confined OpenCode session per epic, run
/// in the target repo with the `reviewer.md` prompt and the strongest route.
pub struct LiveReviewerSession {
    live: LiveSession,
}

impl LiveReviewerSession {
    /// Assemble the live session config (materializing the sandbox profile),
    /// using the same bound stop control passed for admission (`agalma-5fy.5`).
    pub fn build(
        state_dir: &Path,
        proxy_url: Option<String>,
        paid_available: bool,
        stop: Rc<dyn StopControl>,
    ) -> Result<Self, ContractError> {
        Ok(LiveReviewerSession {
            live: LiveSession::build(state_dir, proxy_url, paid_available, stop)?,
        })
    }
}

impl ReviewerSession for LiveReviewerSession {
    fn run(&self, request: &ReviewRequest) -> Result<String, ContractError> {
        let key = sanitize(&request.epic);
        let dir_key = format!("review/{key}");
        // The output contract is written by the confined session under the
        // session's attempt root (conductor-owned, outside the target repo).
        let attempt_scratch =
            crate::session::attempt_root(&request.state_dir, &dir_key).join("review");
        std::fs::create_dir_all(&attempt_scratch).map_err(|e| {
            ContractError::KnownFailure(format!("mkdir {}: {e}", attempt_scratch.display()))
        })?;
        let output_path = attempt_scratch.join("output.txt");

        let spec = SessionSpec {
            role: "reviewer".to_string(),
            model: request.model.clone(),
            repo: request.repo.clone(),
            dir_key,
            socket_name: format!("harness-review-{key}.sock"),
            operation_id: OperationId::new(format!("op:review:{}", request.epic)),
            prompt_path: request.prompt_path.clone(),
            input_ref: ArtifactRef::derive("reviewer-turn"),
            tool_policy_ref: ArtifactRef::derive("tool-policy-reviewer"),
            genome_ref: ArtifactRef::derive("genome"),
            constraints_ref: ArtifactRef::derive("platform-constraints"),
            output_path: Some(output_path),
            error_label: format!("reviewer session for {}", request.epic),
        };
        let result = crate::session::run_session(&self.live, &spec);
        let _ = std::fs::remove_dir_all(&attempt_scratch);
        Ok(result?.unwrap_or_default())
    }
}

/// Production entry point: build the planner gate and the live reviewer session,
/// then run the review pass. Skips (with a log line) when a stop is active or
/// the strong route is unavailable — never a silent pass or downgrade.
pub fn run_review(
    repo: &Path,
    state_dir: &Path,
    paid_available: bool,
    proxy_url: Option<String>,
    override_model: Option<&str>,
) -> Result<ReviewReport, ContractError> {
    let stop = bind_stop_control(state_dir);
    run_review_with_stop(
        repo,
        state_dir,
        paid_available,
        proxy_url,
        override_model,
        stop,
    )
}

/// [`run_review`] honoring an explicit (already-bound) stop control.
///
/// The **same** control is used for admission and passed through the reviewer
/// session seam. The work loop already holds a bound ledger handle and MUST
/// prefer this entry with a [`crate::stop::BoundStopControl`] built from that
/// handle so the review gate reads the *same* durable stop state (`agalma-5fy.5`).
pub fn run_review_with_stop(
    repo: &Path,
    state_dir: &Path,
    paid_available: bool,
    proxy_url: Option<String>,
    override_model: Option<&str>,
    stop: Rc<dyn StopControl>,
) -> Result<ReviewReport, ContractError> {
    if let Some(reason) = stop.decision().reason() {
        eprintln!("agalma: review gate skipped: {reason}");
        return Ok(ReviewReport {
            skipped_reason: Some(reason.to_string()),
            stopped: true,
            ..ReviewReport::default()
        });
    }
    let gate = PlannerGate::from_env(paid_available, override_model);
    if !gate.available() {
        let reason = gate.unavailable_reason();
        eprintln!("agalma: review gate skipped: {reason}");
        return Ok(ReviewReport {
            skipped_reason: Some(reason),
            ..ReviewReport::default()
        });
    }
    let session =
        LiveReviewerSession::build(state_dir, proxy_url, paid_available, Rc::clone(&stop))?;
    review_epics_with_stop(repo, state_dir, &gate, &session, &*stop)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_verdict_accepts_the_exact_approval_marker() {
        assert_eq!(
            parse_verdict("some reasoning\nREVIEW: APPROVED\n").unwrap(),
            ReviewVerdict::Approved
        );
    }

    #[test]
    fn parse_verdict_accepts_drift_with_feedback() {
        let verdict = parse_verdict("REVIEW: DRIFT missing acceptance for X").unwrap();
        assert_eq!(
            verdict,
            ReviewVerdict::Drift {
                feedback: "missing acceptance for X".to_string()
            }
        );
    }

    #[test]
    fn parse_verdict_absent_marker_is_an_error() {
        let err = parse_verdict("looks good to me, no marker here").unwrap_err();
        assert!(err.to_string().contains("never a silent pass"), "{err}");
    }

    #[test]
    fn parse_verdict_multiple_markers_are_an_error() {
        let err = parse_verdict("REVIEW: APPROVED\nREVIEW: DRIFT nope").unwrap_err();
        assert!(err.to_string().contains("exactly one"), "{err}");
    }

    #[test]
    fn parse_verdict_drift_without_feedback_is_an_error() {
        let err = parse_verdict("REVIEW: DRIFT   ").unwrap_err();
        assert!(err.to_string().contains("no feedback"), "{err}");
    }

    #[test]
    fn parse_verdict_unknown_marker_is_an_error() {
        let err = parse_verdict("REVIEW: MAYBE").unwrap_err();
        assert!(err.to_string().contains("unrecognized"), "{err}");
    }

    #[test]
    fn review_start_sha_reads_the_baseline_label() {
        let labels = vec!["staged".to_string(), "review-start:abc123".to_string()];
        assert_eq!(review_start_sha(&labels).as_deref(), Some("abc123"));
        assert!(review_start_sha(&["staged".to_string()]).is_none());
    }

    #[test]
    fn reviewer_prompt_names_the_exact_epic_and_diff() {
        let text = reviewer_prompt(
            "agalma-aaa",
            Path::new("/repos/alpha"),
            "deadbeef",
            "cafef00d",
            Path::new("/state/review/agalma-aaa/diff.patch"),
        );
        assert!(text.contains("epic: agalma-aaa"), "{text}");
        assert!(text.contains("repository: /repos/alpha"), "{text}");
        assert!(text.contains("diff baseline: deadbeef"), "{text}");
        assert!(text.contains("reviewed head: cafef00d"), "{text}");
        assert!(
            text.contains("merged diff: /state/review/agalma-aaa/diff.patch"),
            "{text}"
        );
        // The canonical role text (with its marker contract) is preserved.
        assert!(text.contains(crate::REVIEWER_PROMPT.trim_end()), "{text}");
    }
}
