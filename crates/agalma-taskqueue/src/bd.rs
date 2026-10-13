//! bd backlog adapter.
//!
//! `BdTaskQueue` implements [`TaskQueueApi`] on top of the `bd` CLI. The CLI,
//! its JSON shapes, and the store layout are private to this crate: callers see
//! only canonical [`Task`] records and the projection methods.
//!
//! Every invocation targets an explicit store with `--db <workspace>/.beads`
//! and runs with `current_dir(workspace)`, so a test or conductor can never
//! accidentally read or write a different repository's real backlog.

use std::path::PathBuf;
use std::process::Command;

use agalma_contracts::{
    ClaimEvidence, ContractError, ReadyTasks, SkipReason, SkippedIssue, Task, TaskId, TaskQueueApi,
};
use agalma_ipc::IssueProjection;
use serde::Deserialize;

use crate::block::parse_task_block;

/// Label marking a task terminal-but-unfinished (design §"Task model").
pub const PARKED_LABEL: &str = "parked";
/// Label marking a staged decomposition child (CP.3): hidden from `bd ready`
/// until a human `bd undefer`s it.
///
/// `staged` marks *provenance* (a designer-authored decomposition child), never
/// authorization: it survives both validator approval and human `bd undefer`.
pub const STAGED_LABEL: &str = "staged";
/// Label a passing decomposition gate applies to every staged child (CP.3):
/// the validator-approval bit. Not sufficient on its own — see
/// [`BdIssue::is_ready_candidate`].
pub const DECOMP_APPROVED_LABEL: &str = "decomp-approved";
/// Label a failing decomposition gate applies to every staged child (CP.3): a
/// rejected child never becomes ready, whatever its status or defer state.
pub const DECOMP_REJECTED_LABEL: &str = "decomp-rejected";
/// Label marking an epic escalated to a human (CP.3): excluded from `bd ready`.
pub const HUMAN_LABEL: &str = "human";
/// Label marking a decomposition rework task (CP.3): excluded from `bd ready`.
pub const DECOMP_FIX_LABEL: &str = "decomp-fix";
/// Label on an epic while one designer attempt is in flight (CP.3 recovery).
///
/// Set on the epic immediately before the designer session starts and removed
/// once the attempt's verdict is projected. A durable present marker at the
/// next sweep means the previous attempt was interrupted (process death or
/// cancelled session): the sweep reconciles the partial children instead of
/// spawning a second designer session over the same epic.
pub const DECOMP_INFLIGHT_LABEL: &str = "decomp-inflight";
/// Prefix of the durable per-epic attempt counter label (`decomp-attempts:<n>`).
///
/// Records how many designer attempts an epic has consumed so the retry bound
/// survives restart even before a `decomp-fix` rework task exists. Monotonic;
/// the sweep reads the maximum matching value.
pub const DECOMP_ATTEMPTS_PREFIX: &str = "decomp-attempts:";
/// Prefix of the per-rework-task label naming the attempt it belongs to
/// (`decomp-attempt:<n>`). Lets the gate reuse an attempt's `decomp-fix` task
/// when a crash lands between rework creation and verdict projection, instead
/// of fabricating a second rework task for the same attempt.
pub const DECOMP_ATTEMPT_PREFIX: &str = "decomp-attempt:";

/// One open epic as the CP.3 sweep sees it.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct EpicRecord {
    pub id: String,
    pub title: String,
    pub labels: Vec<String>,
}

/// A dependency edge on an issue, as `bd show --json` reports it.
///
/// `bd list --json` and `bd show --json` use different shapes under the same
/// `dependencies` key: the list shape carries `depends_on_id`/`type`, the show
/// shape nests a full issue object with `id`/`dependency_type`. Both are
/// normalized here by [`BdDependency::target`] and [`BdDependency::edge_kind`].
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct TaskDependency {
    /// The other issue's id.
    pub id: String,
    /// The edge kind (`blocks`, `parent-child`, ...) when recorded.
    pub kind: Option<String>,
}

/// Full issue context for immutable task-package construction (SF.2).
///
/// Unlike [`TaskQueueApi::get`], this does not require a fenced task block: it
/// returns the raw body/design/notes/acceptance, the parent and dependency
/// edges, and comment/handoff text.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct TaskDefinition {
    /// Backlog identity (bd issue id).
    pub task_id: TaskId,
    /// Human title (issue title).
    pub title: String,
    /// Issue description/body exactly as authored.
    pub description: String,
    /// Issue design section, when present.
    pub design: Option<String>,
    /// Issue notes section, when present.
    pub notes: Option<String>,
    /// Issue acceptance criteria text, when present.
    pub acceptance_criteria: Option<String>,
    /// Parent epic id, derived from the nested `parent-child` edge (or the
    /// top-level `parent` field).
    pub parent: Option<String>,
    /// Non-parent dependency edges (e.g. `blocks`).
    pub dependencies: Vec<TaskDependency>,
    /// Comment bodies, oldest first (`--include-comments`).
    pub comments: Vec<String>,
}

/// Task backlog adapter backed by the `bd` CLI.
pub struct BdTaskQueue {
    /// Working directory for `bd` (the repository owning the backlog).
    workspace: PathBuf,
    /// Explicit store passed as `--db`; `workspace/.beads`.
    beads_dir: PathBuf,
    /// The `bd` executable (overridable for tests).
    program: String,
}

impl BdTaskQueue {
    /// Create an adapter for the backlog in `workspace`.
    pub fn new(workspace: impl Into<PathBuf>) -> Self {
        let workspace = workspace.into();
        let beads_dir = workspace.join(".beads");
        Self {
            workspace,
            beads_dir,
            program: "bd".to_string(),
        }
    }

    /// Override the `bd` executable path.
    pub fn with_program(mut self, program: impl Into<String>) -> Self {
        self.program = program.into();
        self
    }

    /// Run `bd` with the shared flags and captured output.
    fn bd(&self, args: &[&str]) -> Result<BdOutput, ContractError> {
        let output = Command::new(&self.program)
            .arg("--db")
            .arg(&self.beads_dir)
            .args(args)
            .current_dir(&self.workspace)
            .env("BD_NON_INTERACTIVE", "1")
            .env("GIT_TERMINAL_PROMPT", "0")
            .output()
            .map_err(|e| {
                ContractError::KnownFailure(format!(
                    "failed to spawn `{} {}`: {e}",
                    self.program,
                    args.join(" ")
                ))
            })?;
        Ok(BdOutput {
            success: output.status.success(),
            stdout: String::from_utf8_lossy(&output.stdout).into_owned(),
            stderr: String::from_utf8_lossy(&output.stderr).into_owned(),
        })
    }

    /// Run `bd`, failing on a non-zero exit with captured stderr.
    fn bd_ok(&self, args: &[&str]) -> Result<String, ContractError> {
        let out = self.bd(args)?;
        if !out.success {
            return Err(ContractError::KnownFailure(format!(
                "`bd {}` failed: {}",
                args.join(" "),
                out.stderr.trim()
            )));
        }
        Ok(out.stdout)
    }

    /// Read an issue's current projection (status, assignee, labels, comment
    /// bodies). Used by the ledger-vs-projection reconcile (M1.6).
    pub fn projection(&self, id: &TaskId) -> Result<IssueProjection, ContractError> {
        let stdout = self.bd_ok(&["show", id.as_str(), "--json", "--include-comments"])?;
        let issue = parse_issues(&stdout)?
            .into_iter()
            .next()
            .ok_or_else(|| ContractError::KnownFailure(format!("task {id}: not found")))?;
        Ok(issue.into_projection())
    }

    /// Read the full issue definition for immutable task-package construction
    /// (SF.2): title, body, design, notes, acceptance, parent and dependency
    /// edges, and comment bodies.
    ///
    /// Unlike [`TaskQueueApi::get`], no fenced task block is required. The nested
    /// `parent-child` dependency edge is lifted to [`TaskDefinition::parent`];
    /// the remaining edges become [`TaskDefinition::dependencies`].
    pub fn task_definition(&self, id: &TaskId) -> Result<TaskDefinition, ContractError> {
        let stdout = self.bd_ok(&["show", id.as_str(), "--json", "--include-comments"])?;
        let issue = parse_issues(&stdout)?
            .into_iter()
            .next()
            .ok_or_else(|| ContractError::KnownFailure(format!("task {id}: not found")))?;
        Ok(issue.into_definition())
    }

    /// Set an issue's status. `closed` goes through `bd close` (which records a
    /// reason); every other status is a direct `bd update --status`.
    pub fn set_status(&mut self, id: &TaskId, status: &str) -> Result<(), ContractError> {
        if status == "closed" {
            return self.close_task(id, "reconcile: ledger is source of truth");
        }
        self.bd_ok(&["update", id.as_str(), "--status", status])?;
        Ok(())
    }

    /// Remove a label from an issue (no-op when absent).
    pub fn remove_label(&mut self, id: &TaskId, label: &str) -> Result<(), ContractError> {
        self.bd_ok(&["update", id.as_str(), "--remove-label", label])?;
        Ok(())
    }

    /// List an epic's children as decomposition records, including deferred and
    /// staged children (`--all` lifts the default status filter). Used by the
    /// CP.1 validator (`agalma plan-check`).
    pub fn plan_children(
        &self,
        parent: &str,
    ) -> Result<Vec<crate::decomp::ChildTask>, ContractError> {
        let stdout = self.bd_ok(&["list", "--parent", parent, "--all", "--json", "--limit", "0"])?;
        Ok(parse_issues(&stdout)?
            .into_iter()
            .map(BdIssue::into_child_task)
            .collect())
    }

    /// Open epics (type `epic`, status `open`), excluding any already escalated
    /// to a human (`human` label). Used by the CP.3 epic sweep to find
    /// undecomposed or rejected epics.
    pub fn open_epics(&self) -> Result<Vec<EpicRecord>, ContractError> {
        let stdout = self.bd_ok(&[
            "list",
            "--type",
            "epic",
            "--status",
            "open",
            "--exclude-label",
            HUMAN_LABEL,
            "--json",
            "--limit",
            "0",
        ])?;
        Ok(parse_issues(&stdout)?
            .into_iter()
            .map(|issue| EpicRecord {
                id: issue.id,
                title: issue.title,
                labels: issue.labels.unwrap_or_default(),
            })
            .collect())
    }

    /// Create a task issue via `bd create`, returning its id. `parent` attaches
    /// it under an epic; `defer` (a `YYYY-MM-DD` date) hides it from `bd ready`
    /// by construction; `labels` is a comma-joined label list.
    pub fn create_task(
        &self,
        title: &str,
        description: &str,
        parent: Option<&str>,
        priority: u8,
        labels: &[&str],
        defer: Option<&str>,
    ) -> Result<String, ContractError> {
        let priority = priority.to_string();
        let mut args: Vec<String> = vec![
            "create".to_string(),
            title.to_string(),
            "-t".to_string(),
            "task".to_string(),
            "-p".to_string(),
            priority,
            "--description".to_string(),
            description.to_string(),
            // bd inherits a parent's labels onto new children; a rework task must
            // carry only its own bookkeeping labels, never the epic's planning
            // markers (`decomp-inflight`, `decomp-attempts:*`) or a stale verdict.
            "--no-inherit-labels".to_string(),
            "--json".to_string(),
        ];
        if let Some(parent) = parent {
            args.push("--parent".to_string());
            args.push(parent.to_string());
        }
        if !labels.is_empty() {
            args.push("--labels".to_string());
            args.push(labels.join(","));
        }
        if let Some(defer) = defer {
            args.push("--defer".to_string());
            args.push(defer.to_string());
        }
        let refs: Vec<&str> = args.iter().map(String::as_str).collect();
        let stdout = self.bd_ok(&refs)?;
        let issue = parse_issues(&stdout)?.into_iter().next().ok_or_else(|| {
            ContractError::KnownFailure("bd create returned no issue".to_string())
        })?;
        Ok(issue.id)
    }
}

impl TaskQueueApi for BdTaskQueue {
    fn ready_tasks(&self) -> Result<ReadyTasks, ContractError> {
        // Ready contract (CP.3): never hand epics or `human`/`decomp-fix`/
        // `parked` issues to the claim path, and never admit a task whose
        // blocking dependency is still open. `--ready` applies bd's
        // blocker-aware ready-work semantics (the `bd ready` query) so an
        // unfinished prerequisite keeps its children out; bd filters below, and
        // [`BdIssue::is_ready_candidate`] re-checks defensively so the
        // projection never depends on the CLI's flag semantics.
        //
        // `staged` is deliberately NOT excluded here: a staged child that has
        // passed the validator gate AND been human-authorized (`bd undefer`,
        // which clears the defer date) must surface. The predicate below keeps
        // still-deferred, rejected, and never-validated children out.
        let stdout = self.bd_ok(&[
            "list",
            "--json",
            "--ready",
            "--status",
            "open",
            "--exclude-type",
            "epic",
            "--exclude-label",
            "parked,human,decomp-fix,decomp-rejected",
            "--limit",
            "0",
        ])?;
        let issues = parse_issues(&stdout)?;

        let mut tasks = Vec::new();
        let mut skipped = Vec::new();
        for issue in issues {
            let id = issue.id.clone();
            if !issue.is_ready_candidate() {
                continue;
            }
            match parse_task_block(&issue.description) {
                Ok(Some(block)) => tasks.push(issue.into_task(block)),
                Ok(None) => skipped.push(SkippedIssue {
                    task_id: TaskId::new(id),
                    reason: SkipReason::MissingBlock,
                }),
                Err(e) => {
                    return Err(ContractError::KnownFailure(format!("task {id}: {e}")));
                }
            }
        }
        Ok(ReadyTasks { tasks, skipped })
    }

    fn get(&self, id: &TaskId) -> Result<Task, ContractError> {
        let stdout = self.bd_ok(&["show", id.as_str(), "--json"])?;
        let issue = parse_issues(&stdout)?
            .into_iter()
            .next()
            .ok_or_else(|| ContractError::KnownFailure(format!("task {id}: not found")))?;
        let block = parse_task_block(&issue.description)
            .map_err(|e| ContractError::KnownFailure(format!("task {id}: {e}")))?
            .ok_or_else(|| {
                ContractError::KnownFailure(format!("task {id}: no fenced task block"))
            })?;
        Ok(issue.into_task(block))
    }

    fn claim(&mut self, id: &TaskId) -> Result<ClaimEvidence, ContractError> {
        let stdout = self.bd_ok(&["update", id.as_str(), "--claim", "--json"])?;
        let issue = parse_issues(&stdout)?.into_iter().next().ok_or_else(|| {
            ContractError::KnownFailure(format!("task {id}: claim returned no issue"))
        })?;
        Ok(ClaimEvidence {
            task_id: TaskId::new(issue.id),
            status: issue.status,
            assignee: issue.assignee,
            started_at: issue.started_at,
        })
    }

    fn comment(&mut self, id: &TaskId, text: &str) -> Result<(), ContractError> {
        self.bd_ok(&["comment", id.as_str(), text])?;
        Ok(())
    }

    fn close_task(&mut self, id: &TaskId, reason: &str) -> Result<(), ContractError> {
        self.bd_ok(&["close", id.as_str(), "--reason", reason])?;
        Ok(())
    }

    fn add_label(&mut self, id: &TaskId, label: &str) -> Result<(), ContractError> {
        self.bd_ok(&["update", id.as_str(), "--add-label", label])?;
        Ok(())
    }
}

struct BdOutput {
    success: bool,
    stdout: String,
    stderr: String,
}

/// One comment on an issue (only the body text is consumed).
#[derive(Debug, Deserialize)]
struct BdComment {
    #[serde(default)]
    text: Option<String>,
}

/// A dependency edge as `bd list --json` reports it. `bd show --json` nests a
/// different (full-issue) shape under the same key, so every field is optional
/// and non-matching edges are ignored — this keeps the shared `parse_issues`
/// usable for both payloads.
#[derive(Debug, Deserialize)]
struct BdDependency {
    /// The issue id from the list shape (`depends_on_id`) or the nested
    /// show-shape issue object (`id`).
    #[serde(default)]
    depends_on_id: Option<String>,
    #[serde(default)]
    id: Option<String>,
    /// Edge kind from the list shape (`type`).
    #[serde(default, rename = "type")]
    kind: Option<String>,
    /// Edge kind from the nested show shape (`dependency_type`).
    #[serde(default)]
    dependency_type: Option<String>,
}

impl BdDependency {
    /// The other issue's id, whichever shape supplied it.
    fn target(&self) -> Option<&str> {
        self.depends_on_id.as_deref().or(self.id.as_deref())
    }

    /// The edge kind, whichever shape supplied it.
    fn edge_kind(&self) -> Option<&str> {
        self.kind.as_deref().or(self.dependency_type.as_deref())
    }
}

/// Subset of the bd issue JSON the adapter consumes.
#[derive(Debug, Deserialize)]
struct BdIssue {
    id: String,
    title: String,
    #[serde(default)]
    description: String,
    #[serde(default)]
    design: Option<String>,
    #[serde(default)]
    notes: Option<String>,
    #[serde(default)]
    acceptance_criteria: Option<String>,
    #[serde(default)]
    parent: Option<String>,
    #[serde(default)]
    issue_type: Option<String>,
    status: String,
    #[serde(default)]
    assignee: Option<String>,
    #[serde(default)]
    started_at: Option<String>,
    /// bd defer marker: present while the issue is snoozed (created with
    /// `--defer`, or after an accidental status flip). Human `bd undefer`
    /// clears it. Used to distinguish real human authorization from a mere
    /// status change.
    #[serde(default)]
    defer_until: Option<String>,
    #[serde(default)]
    labels: Option<Vec<String>>,
    #[serde(default)]
    dependencies: Option<Vec<BdDependency>>,
    #[serde(default)]
    comments: Option<Vec<BdComment>>,
}

impl BdIssue {
    fn has_label(&self, label: &str) -> bool {
        self.labels
            .as_deref()
            .is_some_and(|labels| labels.iter().any(|l| l == label))
    }

    /// Whether this issue may be handed to the claim path.
    ///
    /// Approval is two independent bits (design: `docs/planning-pipeline.md`,
    /// "Ready contract"):
    ///
    /// - **validator approval** — the [`DECOMP_APPROVED_LABEL`] the sweep gate
    ///   adds on a clean decomposition (never with [`DECOMP_REJECTED_LABEL`]);
    /// - **human execution authorization** — `bd undefer <child>`, which
    ///   restores `status=open` and clears the defer date while preserving
    ///   labels (including `staged`).
    ///
    /// Consequently a `staged` child is consumable only when it is approved and
    /// no longer deferred. `staged` alone (never validated), `staged` +
    /// `decomp-rejected`, and any issue still carrying a defer date stay hidden
    /// — so an accidental `bd update --status open` (which leaves the defer
    /// date in place) cannot release unapproved work. Ordinary non-staged open
    /// work needs neither label.
    fn is_ready_candidate(&self) -> bool {
        if self.status != "open" || self.issue_type.as_deref() == Some("epic") {
            return false;
        }
        // A set defer date means no human has run `bd undefer` yet; bd's native
        // ready semantics also honor it. `undefer` clears the field.
        if self.defer_until.is_some() {
            return false;
        }
        if self.has_label(PARKED_LABEL)
            || self.has_label(HUMAN_LABEL)
            || self.has_label(DECOMP_FIX_LABEL)
        {
            return false;
        }
        // A rejected child never executes, staged or not.
        if self.has_label(DECOMP_REJECTED_LABEL) {
            return false;
        }
        // A staged child additionally needs validator approval.
        if self.has_label(STAGED_LABEL) && !self.has_label(DECOMP_APPROVED_LABEL) {
            return false;
        }
        true
    }

    fn into_projection(self) -> IssueProjection {
        IssueProjection {
            status: self.status,
            assignee: self.assignee,
            labels: self.labels.unwrap_or_default(),
            comments: self
                .comments
                .unwrap_or_default()
                .into_iter()
                .filter_map(|c| c.text)
                .collect(),
        }
    }

    fn into_task(self, block: crate::block::TaskBlock) -> Task {
        Task {
            task_id: TaskId::new(self.id),
            title: self.title,
            directive: block.directive,
            target: block.target,
            base_ref: block.base_ref,
            acceptance: block.acceptance,
            budget_usd: block.budget_usd,
            priority: block.priority,
            family: block.family,
            max_attempts: block.max_attempts,
            raw_block: block.raw_block,
        }
    }

    /// Project an issue into a decomposition child record. Only `blocks` edges
    /// (sibling dependencies) are kept; the `parent-child` edge is dropped.
    fn into_child_task(self) -> crate::decomp::ChildTask {
        let depends_on = self
            .dependencies
            .unwrap_or_default()
            .into_iter()
            .filter(|edge| edge.edge_kind() == Some("blocks"))
            .filter_map(|edge| edge.target().map(str::to_string))
            .collect();
        crate::decomp::ChildTask {
            id: self.id,
            title: self.title,
            description: self.description,
            design: self.design,
            labels: self.labels.unwrap_or_default(),
            depends_on,
        }
    }

    /// Project an issue into the full definition for task-package construction.
    ///
    /// The nested `parent-child` edge is lifted to the parent; other edges are
    /// kept as dependencies. A top-level `parent` field (when present) wins.
    fn into_definition(self) -> TaskDefinition {
        let mut parent = self.parent.clone();
        let mut dependencies = Vec::new();
        for edge in self.dependencies.unwrap_or_default() {
            let Some(target) = edge.target().map(str::to_string) else {
                continue;
            };
            let kind = edge.edge_kind().map(str::to_string);
            if kind.as_deref() == Some("parent-child") {
                if parent.is_none() {
                    parent = Some(target);
                }
                continue;
            }
            dependencies.push(TaskDependency { id: target, kind });
        }
        TaskDefinition {
            task_id: TaskId::new(self.id),
            title: self.title,
            description: self.description,
            design: self.design,
            notes: self.notes,
            acceptance_criteria: self.acceptance_criteria,
            parent,
            dependencies,
            comments: self
                .comments
                .unwrap_or_default()
                .into_iter()
                .filter_map(|c| c.text)
                .collect(),
        }
    }
}

/// Parse a bd JSON payload that is either an array of issues or a single issue.
fn parse_issues(stdout: &str) -> Result<Vec<BdIssue>, ContractError> {
    let trimmed = stdout.trim();
    if trimmed.is_empty() {
        return Ok(Vec::new());
    }
    if let Ok(issues) = serde_json::from_str::<Vec<BdIssue>>(trimmed) {
        return Ok(issues);
    }
    let one: BdIssue = serde_json::from_str(trimmed)
        .map_err(|e| ContractError::KnownFailure(format!("cannot parse bd JSON: {e}")))?;
    Ok(vec![one])
}

#[cfg(test)]
mod tests {
    use super::*;

    /// An open, unlabelled, undeferred issue: the permissive baseline the
    /// predicate narrows from.
    fn issue() -> BdIssue {
        BdIssue {
            id: "x".to_string(),
            title: "x".to_string(),
            description: String::new(),
            design: None,
            notes: None,
            acceptance_criteria: None,
            parent: None,
            issue_type: Some("task".to_string()),
            status: "open".to_string(),
            assignee: None,
            started_at: None,
            defer_until: None,
            labels: None,
            dependencies: None,
            comments: None,
        }
    }

    fn with_labels(labels: &[&str]) -> BdIssue {
        BdIssue {
            labels: Some(labels.iter().map(|l| l.to_string()).collect()),
            ..issue()
        }
    }

    #[test]
    fn ordinary_open_undeferred_work_is_ready() {
        assert!(issue().is_ready_candidate());
        assert!(with_labels(&["difficulty:low"]).is_ready_candidate());
    }

    #[test]
    fn deferred_rejected_and_special_labels_stay_hidden() {
        let mut deferred = issue();
        deferred.defer_until = Some("2099-01-01T00:00:00Z".to_string());
        assert!(
            !deferred.is_ready_candidate(),
            "a set defer date means no human `bd undefer` yet"
        );

        let mut epic = issue();
        epic.issue_type = Some("epic".to_string());
        assert!(!epic.is_ready_candidate());

        for label in [PARKED_LABEL, HUMAN_LABEL, DECOMP_FIX_LABEL, DECOMP_REJECTED_LABEL] {
            assert!(
                !with_labels(&[label]).is_ready_candidate(),
                "{label} must stay hidden"
            );
        }
    }

    #[test]
    fn staged_children_require_both_approval_and_authorization() {
        // Never validated: hidden.
        assert!(!with_labels(&[STAGED_LABEL]).is_ready_candidate());
        // Validator-approved but still deferred: hidden (no human undefer).
        let mut approved_deferred = with_labels(&[STAGED_LABEL, DECOMP_APPROVED_LABEL]);
        approved_deferred.defer_until = Some("2099-01-01T00:00:00Z".to_string());
        assert!(!approved_deferred.is_ready_candidate());
        // Validator-approved + human-undeferred: ready.
        assert!(with_labels(&[STAGED_LABEL, DECOMP_APPROVED_LABEL]).is_ready_candidate());
        // Rejected alongside approval: still hidden.
        assert!(
            !with_labels(&[STAGED_LABEL, DECOMP_APPROVED_LABEL, DECOMP_REJECTED_LABEL])
                .is_ready_candidate()
        );
    }

    #[test]
    fn accidental_status_change_with_defer_intact_stays_hidden() {
        // `bd update --status open` flips the status but leaves the defer date,
        // so never-validated and rejected children stay invisible.
        let mut never_validated = with_labels(&[STAGED_LABEL]);
        never_validated.status = "open".to_string();
        never_validated.defer_until = Some("2099-01-01T00:00:00Z".to_string());
        assert!(!never_validated.is_ready_candidate());

        let mut rejected = with_labels(&[STAGED_LABEL, DECOMP_REJECTED_LABEL]);
        rejected.status = "open".to_string();
        rejected.defer_until = Some("2099-01-01T00:00:00Z".to_string());
        assert!(!rejected.is_ready_candidate());
    }

    #[test]
    fn show_shaped_nested_dependencies_and_parent_parse() {
        // `bd show --json` nests full issue objects with `id`/`dependency_type`.
        let json = r#"[
          {
            "id": "t.1",
            "title": "T",
            "description": "Task body.",
            "design": "Design text.",
            "notes": "Notes text.",
            "acceptance_criteria": "Acceptance text.",
            "status": "open",
            "dependencies": [
              { "id": "epic.1", "dependency_type": "parent-child" },
              { "id": "dep.1", "dependency_type": "blocks" }
            ],
            "comments": [
              { "id": "c1", "text": "handoff one" },
              { "id": "c2", "text": "handoff two" }
            ]
          }
        ]"#;
        let definition = parse_issues(json)
            .expect("parse show shape")
            .into_iter()
            .next()
            .expect("one issue")
            .into_definition();
        assert_eq!(definition.parent.as_deref(), Some("epic.1"));
        assert_eq!(
            definition.dependencies,
            vec![TaskDependency {
                id: "dep.1".to_string(),
                kind: Some("blocks".to_string()),
            }]
        );
        assert_eq!(
            definition.comments,
            vec!["handoff one".to_string(), "handoff two".to_string()]
        );
        assert_eq!(definition.acceptance_criteria.as_deref(), Some("Acceptance text."));
        assert_eq!(definition.design.as_deref(), Some("Design text."));
        assert_eq!(definition.notes.as_deref(), Some("Notes text."));
        assert_eq!(definition.title, "T");
    }

    #[test]
    fn list_shaped_dependency_edges_still_parse_blocks() {
        // `bd list --json` uses `depends_on_id`/`type`; the normalized helpers
        // keep decomposition children working.
        let json = r#"[
          {
            "id": "t.1",
            "title": "T",
            "status": "open",
            "dependencies": [
              { "depends_on_id": "dep.1", "type": "blocks" },
              { "depends_on_id": "epic.1", "type": "parent-child" }
            ]
          }
        ]"#;
        let child = parse_issues(json)
            .expect("parse list shape")
            .into_iter()
            .next()
            .expect("one issue")
            .into_child_task();
        assert_eq!(child.depends_on, vec!["dep.1".to_string()]);
    }
}

