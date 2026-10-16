//! bd backlog adapter.
//!
//! `BdTaskQueue` implements [`TaskQueueApi`] on top of the `bd` CLI. The CLI,
//! its JSON shapes, and the store layout are private to this crate: callers see
//! only canonical [`Task`] records and the projection methods.
//!
//! Every invocation targets an explicit store with `--db <workspace>/.beads`
//! and runs with `current_dir(workspace)`, so a test or conductor can never
//! accidentally read or write a different repository's real backlog.

use std::collections::{BTreeMap, BTreeSet};
use std::path::PathBuf;
use std::process::Command;

use agalma_contracts::PackageArtifactRef;
use agalma_contracts::{
    admission_definition_canonical, admission_definition_digest, blocking_prerequisite_ids,
    decide_review_admission, normalize_children, normalize_edges, AdmissionChild, AdmissionEdge,
    AdmissionEdgeType, ClaimEvidence, ContractError, EpicReviewAdmission, ReadyTasks,
    ReviewAdmissionContext, ReviewAdmissionDecision, SkipReason, SkippedIssue, Task, TaskId,
    TaskQueueApi, EPIC_REVIEW_ADMISSION_FORMAT,
};
use agalma_ipc::IssueProjection;
use serde::Deserialize;
use serde_json::Value;

use crate::block::parse_task_block;

/// Label marking a task terminal-but-unfinished (design §"Task model").
pub const PARKED_LABEL: &str = "parked";
/// Label marking a staged decomposition child (CP.3): hidden from `bd ready`
/// until its native human approval gate is resolved.
///
/// `staged` marks *provenance* (a designer-authored decomposition child), never
/// authorization: it survives both validator approval and human gate resolution.
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
/// Label marking a decomposition rework task (CP.3): excluded from *factory*
/// readiness by this label (native `bd ready` does not exclude it intrinsically).
pub const DECOMP_FIX_LABEL: &str = "decomp-fix";
/// Label marking a post-merge review-drift rework task (CP.5): excluded from
/// *factory* readiness by this label (native `bd ready` does not exclude it
/// intrinsically).
pub const DRIFT_FIX_LABEL: &str = "drift-fix";
/// Label marking a native human execution-approval gate issue (CP.3 /
/// `agalma-i5h.8`): the gate never executes, and it is what pins a staged child
/// until a human resolves it.
pub const EXECUTION_APPROVAL_LABEL: &str = "execution-approval";
/// Metadata key on a native human gate naming the task it authorizes
/// (`approval_for=<task-id>`).
pub const APPROVAL_FOR_KEY: &str = "approval_for";
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

/// The immutable review source of an epic (SF.11): its canonical
/// source-definition digest and its design artifact reference.
///
/// The conductor uses [`BdTaskQueue::review_source`] to build the admitted
/// [`ReviewAdmissionContext`] and to verify the supplied design reference still
/// matches the live source definition.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ReviewSource {
    /// The epic's immutable source-definition digest.
    pub definition_digest: String,
    /// The exact canonical-json/v1 definition bytes whose SHA-256 is
    /// `definition_digest`.
    pub definition_canonical: String,
    /// The epic's design artifact reference (path `bd:<id>:design`).
    pub design: PackageArtifactRef,
    /// The exact design bytes whose SHA-256 is `design.digest`.
    pub design_text: String,
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

/// A native human execution-approval gate as the adapter reads it.
///
/// A gate is an ordinary bd issue (conventionally created with
/// `bd gate create --type human --blocks <task>`) that carries the
/// [`HUMAN_LABEL`] + [`EXECUTION_APPROVAL_LABEL`] labels and the
/// `approval_for=<task-id>` metadata, and is pinned to its task by a direct
/// `blocks` edge. `resolved` is true once a human closes it (`bd gate resolve`),
/// which is the explicit human bootstrap authorization for that task.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ApprovalGate {
    /// The gate issue id.
    pub id: String,
    /// The task id the gate authorizes (`approval_for`), when well-formed.
    pub approval_for: Option<String>,
    /// The gate's labels.
    pub labels: Vec<String>,
    /// The bd issue type (must be `gate` for a native gate).
    pub issue_type: Option<String>,
    /// The bd gate await type (must be `human` for a native human gate).
    pub await_type: Option<String>,
    /// Whether the gate has been resolved (closed) by a human.
    pub resolved: bool,
}

impl ApprovalGate {
    /// Whether this issue is a *native* human gate: `issue_type=gate` with
    /// `await_type=human`. A plain task or a timer/automation gate that merely
    /// copied the labels and metadata is rejected.
    pub fn is_native_human(&self) -> bool {
        self.issue_type.as_deref() == Some("gate") && self.await_type.as_deref() == Some("human")
    }

    /// Whether this gate claims execution approval: it carries the
    /// [`EXECUTION_APPROVAL_LABEL`] or an `approval_for` target. A native gate
    /// claiming neither is an ordinary prerequisite gate, not an approval
    /// association — it blocks the child like any dependency and is governed by
    /// bd's normal ready semantics.
    pub fn claims_execution_approval(&self) -> bool {
        self.approval_for.is_some() || self.labels.iter().any(|l| l == EXECUTION_APPROVAL_LABEL)
    }

    /// Whether two discovery observations describe the same gate. Label order is
    /// normalized so the same native gate returned with reordered labels is one
    /// observation, not a spurious conflict; any other differing field is a
    /// genuinely different observation.
    pub fn same_observation(&self, other: &ApprovalGate) -> bool {
        if self.id != other.id
            || self.approval_for != other.approval_for
            || self.issue_type != other.issue_type
            || self.await_type != other.await_type
            || self.resolved != other.resolved
        {
            return false;
        }
        let mut a = self.labels.clone();
        let mut b = other.labels.clone();
        a.sort();
        b.sort();
        a == b
    }
}

/// How a staged child relates to its native human approval gate.
///
/// Only [`ApprovalState::Resolved`] authorizes release. Every other variant is
/// fail-closed: a missing, ambiguous, or malformed association is *not* treated
/// as approval, and the conductor never invents or recreates one.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ApprovalState {
    /// No associated gate was found.
    Missing,
    /// More than one associated gate was found (ambiguous).
    Conflicting,
    /// A gate is associated but is not well formed (labels/metadata mismatch).
    Malformed,
    /// Exactly one well-formed gate exists but a human has not resolved it.
    Unresolved,
    /// Exactly one well-formed gate exists and a human has resolved it.
    Resolved,
}

impl ApprovalState {
    /// Whether the association is well formed (`Resolved` or `Unresolved`): the
    /// sweep requires this before it applies decomposition approval. A malformed
    /// or absent association fails closed.
    pub fn is_well_formed(self) -> bool {
        matches!(self, ApprovalState::Resolved | ApprovalState::Unresolved)
    }
}

/// Classify `child_id`'s native human gate association from its `blocks` edge
/// targets and the backlog's gate records.
///
/// Only gates that [`ApprovalGate::claims_execution_approval`] are approval
/// associations; an unrelated native prerequisite gate (a timer/human gate with
/// neither the `execution-approval` label nor `approval_for`) is an ordinary
/// dependency blocker and is ignored here. Exactly one approval association
/// whose labels carry both [`HUMAN_LABEL`] and [`EXECUTION_APPROVAL_LABEL`],
/// which is a native gate, and whose `approval_for` names `child_id` is
/// well-formed; it is `Resolved` once closed. Zero matches is
/// [`ApprovalState::Missing`], more than one is [`ApprovalState::Conflicting`],
/// and a single non-matching approval association is [`ApprovalState::Malformed`]
/// — all fail closed.
pub fn approval_state(
    child_id: &str,
    blockers: &[String],
    gates: &[ApprovalGate],
) -> ApprovalState {
    let associated: Vec<&ApprovalGate> = gates
        .iter()
        .filter(|gate| blockers.iter().any(|blocker| blocker == &gate.id))
        .filter(|gate| gate.claims_execution_approval())
        .collect();
    match associated.as_slice() {
        [] => ApprovalState::Missing,
        [gate] => {
            let well_formed = gate.is_native_human()
                && gate.approval_for.as_deref() == Some(child_id)
                && gate.labels.iter().any(|l| l == HUMAN_LABEL)
                && gate.labels.iter().any(|l| l == EXECUTION_APPROVAL_LABEL);
            if !well_formed {
                ApprovalState::Malformed
            } else if gate.resolved {
                ApprovalState::Resolved
            } else {
                ApprovalState::Unresolved
            }
        }
        _ => ApprovalState::Conflicting,
    }
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

    /// The epic's immutable review source (SF.11): canonical source-definition
    /// digest plus design artifact reference. Used by the conductor to build the
    /// admitted context and to verify a supplied design reference still matches
    /// the live source.
    pub fn review_source(&self, epic_id: &str) -> Result<ReviewSource, ContractError> {
        let (issue, _value) = self.show_issue_parts(epic_id)?;
        source_from_issue(&issue)
    }

    /// Compute the prerequisite-aware epic review admission projection (SF.11).
    ///
    /// Reads the complete current source/children/edge projection, echoes the
    /// caller's admitted immutable context, and computes the deterministic
    /// decision. Missing/unknown/ambiguous graph data, duplicate edge identities,
    /// a mismatched design context, and a reachable `blocks` cycle all fail
    /// closed.
    pub fn review_admission(
        &self,
        epic_id: &str,
        context: &ReviewAdmissionContext,
    ) -> Result<EpicReviewAdmission, ContractError> {
        let mut malformed = false;
        let (issue, value) = self.show_issue_parts(epic_id)?;
        let source = source_from_issue(&issue)?;

        // Required projection data: the dependency projection must be complete
        // (`dependency_count` present, and the `dependencies` array length must
        // match it). Native bd reports `dependency_count=0` and omits the array
        // for a known-empty graph, which is accepted.
        if !dependency_projection_complete(&value) {
            malformed = true;
        }

        // Complete child set, normalized by id. Native bd omits `labels` for a
        // genuinely unlabeled issue, which is the documented known-empty set;
        // malformed explicit labels and non-issue rows are rejected by the typed
        // parse. Duplicate identities are rejected rather than silently dropped.
        let child_values = parse_value_array(&self.bd_ok(&[
            "list", "--parent", epic_id, "--all", "--json", "--limit", "0",
        ])?)?;
        let mut child_rows: Vec<BdIssue> = Vec::with_capacity(child_values.len());
        for child_value in &child_values {
            let child: BdIssue = serde_json::from_value(child_value.clone()).map_err(|e| {
                ContractError::KnownFailure(format!(
                    "child projection for {epic_id}: invalid JSON: {e}"
                ))
            })?;
            child_rows.push(child);
        }
        child_rows.sort_by(|a, b| a.id.cmp(&b.id));
        let mut children: Vec<AdmissionChild> = Vec::with_capacity(child_rows.len());
        let mut last_id: Option<&str> = None;
        for child in &child_rows {
            if last_id == Some(child.id.as_str()) {
                malformed = true; // duplicate child identity
            }
            last_id = Some(child.id.as_str());
            let definition_digest = admission_definition_digest(
                &child.id,
                &child.title,
                &child.description,
                child.design.as_deref(),
                child.acceptance_criteria.as_deref(),
            )?;
            let mut labels = child.labels.clone().unwrap_or_default();
            labels.sort();
            children.push(AdmissionChild {
                id: child.id.clone(),
                definition_digest,
                status: child.status.clone(),
                labels,
            });
        }

        // Direct edges from the epic. Only `blocks` gates review; `tracks` and
        // parent-child edges are carried but ignored. An unknown edge kind, an
        // empty target, a self-block, a duplicate edge identity (even an
        // identical row), and a `blocks` edge with an unknown observed status
        // all deny.
        let mut edges: Vec<AdmissionEdge> = Vec::new();
        let mut block_targets: Vec<String> = Vec::new();
        let mut seen_edges: BTreeSet<(String, String)> = BTreeSet::new();
        for edge in issue.dependencies.as_deref().unwrap_or_default() {
            if edge.aliases_conflict() {
                malformed = true; // ambiguous target/type identity
                continue;
            }
            let Some(target) = edge.target().map(str::to_string) else {
                malformed = true;
                continue;
            };
            if target.is_empty() {
                malformed = true;
                continue;
            }
            let Some(edge_type) = edge.edge_kind().and_then(AdmissionEdgeType::parse) else {
                malformed = true;
                continue;
            };
            if edge_type == AdmissionEdgeType::Blocks && target == epic_id {
                malformed = true; // cycle: the epic blocks itself
                continue;
            }
            let observed_status = edge.status.clone();
            if edge_type == AdmissionEdgeType::Blocks && observed_status.is_none() {
                malformed = true; // unknown/missing prerequisite target
            }
            if !seen_edges.insert((edge_type.as_str().to_string(), target.clone())) {
                malformed = true; // duplicate edge identity
                continue;
            }
            if edge_type == AdmissionEdgeType::Blocks {
                block_targets.push(target.clone());
            }
            edges.push(AdmissionEdge {
                target_id: target,
                edge_type,
                observed_status,
            });
        }

        // Bounded reachable-cycle detection over the `blocks` graph. A read
        // failure on a reachable node is unknown graph data and denies.
        if !malformed && self.has_blocks_cycle(epic_id, &block_targets) {
            malformed = true;
        }

        let children = normalize_children(&children);
        let edges = normalize_edges(&edges);
        let blocking_ids = blocking_prerequisite_ids(&edges);
        // The design reference must name this epic and match the live source.
        let expected_path = format!("bd:{epic_id}:design");
        if context.design.path != expected_path {
            malformed = true; // wrong source identity
        }
        let context_matches = source.design == context.design;

        let mut decision = decide_review_admission(&children, &edges, malformed);
        if !malformed && !context_matches {
            decision = ReviewAdmissionDecision::Stale;
        }

        let mut record = EpicReviewAdmission {
            format: EPIC_REVIEW_ADMISSION_FORMAT.to_string(),
            epic_id: epic_id.to_string(),
            definition_digest: source.definition_digest,
            design: context.design.clone(),
            base_sha: context.base_sha.clone(),
            candidate_sha: context.candidate_sha.clone(),
            children,
            edges,
            decision,
            blocking_ids,
            observed_at_unix_ms: now_unix_ms(),
            input_digest: String::new(),
        };
        record.input_digest = record.compute_input_digest()?;
        Ok(record)
    }

    /// Read one issue via `bd show <id> --json` together with its raw JSON value
    /// (used to distinguish an absent projection field from a known-empty one).
    fn show_issue_parts(&self, id: &str) -> Result<(BdIssue, Value), ContractError> {
        let stdout = self.bd_ok(&["show", id, "--json"])?;
        let value = parse_first_value(&stdout)?;
        let issue: BdIssue = serde_json::from_value(value.clone()).map_err(|e| {
            ContractError::KnownFailure(format!("issue {id}: invalid projection: {e}"))
        })?;
        if issue.id != id {
            return Err(ContractError::KnownFailure(format!(
                "issue alias conflict: requested {id}, read {}",
                issue.id
            )));
        }
        Ok((issue, value))
    }

    /// The `blocks` edge targets of `id`, requiring the dependency projection to
    /// be complete (so a missing/short graph read is denied, not treated as an
    /// empty leaf).
    fn blocks_targets_of(&self, id: &str) -> Result<Vec<String>, ContractError> {
        let (issue, value) = self.show_issue_parts(id)?;
        if !dependency_projection_complete(&value) {
            return Err(ContractError::KnownFailure(format!(
                "issue {id}: incomplete dependency projection"
            )));
        }
        let mut targets = Vec::new();
        for edge in issue.dependencies.as_deref().unwrap_or_default() {
            if edge.aliases_conflict() {
                return Err(ContractError::KnownFailure(format!(
                    "issue {id}: ambiguous dependency target/type identity"
                )));
            }
            let target = edge.target().ok_or_else(|| {
                ContractError::KnownFailure(format!("issue {id}: dependency without a target"))
            })?;
            match edge.edge_kind().and_then(AdmissionEdgeType::parse) {
                Some(AdmissionEdgeType::Blocks) => targets.push(target.to_string()),
                Some(_) => {}
                None => {
                    return Err(ContractError::KnownFailure(format!(
                        "issue {id}: unknown dependency kind"
                    )))
                }
            }
        }
        Ok(targets)
    }

    /// Whether the `blocks` graph reachable from `epic_id` (starting at
    /// `first_targets`) contains a cycle. Bounded; an unreadable reachable node
    /// is treated as a cycle (fail closed).
    fn has_blocks_cycle(&self, epic_id: &str, first_targets: &[String]) -> bool {
        const MAX_NODES: usize = 4096;
        // 1 = on the current stack (gray), 2 = fully explored (black).
        let mut color: BTreeMap<String, u8> = BTreeMap::new();
        color.insert(epic_id.to_string(), 1);
        let mut count = 1usize;
        let mut stack: Vec<(String, Vec<String>, usize)> =
            vec![(epic_id.to_string(), first_targets.to_vec(), 0)];
        while let Some((node, targets, index)) = stack.pop() {
            if index < targets.len() {
                let target = targets[index].clone();
                stack.push((node, targets, index + 1));
                match color.get(&target).copied() {
                    Some(1) => return true, // back edge into the current stack
                    Some(_) => {}           // already explored (a DAG join)
                    None => {
                        count += 1;
                        if count > MAX_NODES {
                            return true; // unbounded graph: deny
                        }
                        color.insert(target.clone(), 1);
                        match self.blocks_targets_of(&target) {
                            Ok(next) => stack.push((target, next, 0)),
                            Err(_) => return true, // unreadable node: deny
                        }
                    }
                }
            } else {
                color.insert(node, 2);
            }
        }
        false
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
        let stdout = self.bd_ok(&[
            "list", "--parent", parent, "--all", "--json", "--limit", "0",
        ])?;
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

    /// All native human execution-approval gates in the backlog (open and
    /// resolved). Used by the ready predicate and the epic sweep to reconcile
    /// exactly one gate per staged child — never to create or resolve one.
    ///
    /// Discovery is a union of two queries so ambiguity is never silently
    /// reduced away:
    ///
    /// - `--type gate` finds every native gate, including one whose
    ///   `execution-approval` label was stripped (which is therefore
    ///   [`ApprovalState::Malformed`]);
    /// - `--label execution-approval` finds any issue carrying the label, so a
    ///   non-native impostor (a plain task) is classified as malformed too.
    ///
    /// Either way, a second linked association is visible and the exact-one rule
    /// denies it. `bd list` hides native gates by default, so `--include-gates`
    /// is required.
    pub fn approval_gates(&self) -> Result<Vec<ApprovalGate>, ContractError> {
        let mut gates: Vec<ApprovalGate> = Vec::new();
        for args in [
            [
                "list",
                "--type",
                "gate",
                "--all",
                "--include-gates",
                "--json",
                "--limit",
                "0",
            ],
            [
                "list",
                "--label",
                EXECUTION_APPROVAL_LABEL,
                "--all",
                "--include-gates",
                "--json",
                "--limit",
                "0",
            ],
        ] {
            for issue in parse_issues(&self.bd_ok(&args)?)? {
                let gate = issue.into_gate();
                // Deduplicate *identical* observations only (label order
                // normalized). Differing observations for one identity (for
                // example the same gate seen closed in one query and reopened in
                // the other) are both kept, so the exact-one rule sees a conflict
                // and fails closed rather than letting a stale record silently
                // win.
                if gates
                    .iter()
                    .any(|existing| existing.same_observation(&gate))
                {
                    continue;
                }
                gates.push(gate);
            }
        }
        Ok(gates)
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
        // Ready contract (CP.3 / agalma-i5h.8): never hand epics, gate issues,
        // or `human`/`decomp-fix`/`drift-fix`/`parked` issues to the claim path,
        // and never admit a task whose blocking dependency is still open.
        // `--ready` applies bd's blocker-aware ready-work semantics (the
        // `bd ready` query) so an unfinished prerequisite (including an
        // unresolved native approval gate) keeps its children out; bd filters
        // below, and [`BdIssue::is_ready_candidate`] re-checks defensively so
        // the projection never depends on the CLI's flag semantics.
        //
        // `staged` is deliberately NOT excluded here: a staged child that has
        // passed the validator gate AND whose native human approval gate a human
        // has resolved must surface. The predicate below keeps still-blocked,
        // rejected, never-validated, and gate-orphaned children out.
        let stdout = self.bd_ok(&[
            "list",
            "--json",
            "--ready",
            "--status",
            "open",
            "--exclude-type",
            "epic,gate",
            "--exclude-label",
            "parked,human,execution-approval,decomp-fix,drift-fix,decomp-rejected",
            "--limit",
            "0",
        ])?;
        let issues = parse_issues(&stdout)?;
        // Read the gate set once so the (defensive) approval re-check never
        // depends solely on bd's `--ready` flag semantics.
        let gates = self.approval_gates()?;

        let mut tasks = Vec::new();
        let mut skipped = Vec::new();
        for issue in issues {
            let id = issue.id.clone();
            if !issue.is_ready_candidate(&gates) {
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

    fn review_admission(
        &self,
        epic_id: &str,
        context: &ReviewAdmissionContext,
    ) -> Result<EpicReviewAdmission, ContractError> {
        BdTaskQueue::review_admission(self, epic_id, context)
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

/// A dependency edge as `bd list --json`/`bd show --json` reports it. The two
/// payloads use different shapes under the same key: the list shape carries
/// `depends_on_id`/`type` (and an edge-level `metadata` string), the show shape
/// nests a full issue object with `id`/`dependency_type`/`status`/`labels`/
/// `metadata`. Every field is optional and non-matching edges are ignored, which
/// keeps the shared `parse_issues` usable for both payloads.
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
    /// Observed target status from the nested show shape (`status`). Absent in
    /// the list shape and when the target is unknown.
    #[serde(default)]
    status: Option<String>,
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

    /// Whether the two alias fields disagree, so the edge identity is ambiguous.
    ///
    /// A raw row carrying both alias fields with different values (e.g. a
    /// `tracks`/`blocks` kind conflict, or two different target ids) must be
    /// rejected rather than resolved by the legacy preference order — otherwise
    /// a blocking edge could be silently reclassified as non-blocking.
    fn aliases_conflict(&self) -> bool {
        if let (Some(a), Some(b)) = (self.depends_on_id.as_deref(), self.id.as_deref()) {
            if a != b {
                return true;
            }
        }
        if let (Some(a), Some(b)) = (self.kind.as_deref(), self.dependency_type.as_deref()) {
            if a != b {
                return true;
            }
        }
        false
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
    /// bd gate await type (`human`, `timer`, ...); present only on gate issues.
    #[serde(default)]
    await_type: Option<String>,
    status: String,
    #[serde(default)]
    assignee: Option<String>,
    #[serde(default)]
    started_at: Option<String>,
    /// bd defer marker: present while the issue is snoozed (created with
    /// `--defer`, or after an accidental status flip). `bd undefer` clears it.
    /// A real postponement: a set date hides the issue, and clearing a date is
    /// *not* execution authorization (only a resolved native human gate is).
    #[serde(default)]
    defer_until: Option<String>,
    #[serde(default)]
    labels: Option<Vec<String>>,
    /// bd custom metadata object (present on gate issues as `approval_for`).
    #[serde(default)]
    metadata: Option<Value>,
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
    /// - **human execution authorization** — a resolved native human gate: an
    ///   associated gate issue ([`EXECUTION_APPROVAL_LABEL`] + [`HUMAN_LABEL`] +
    ///   `approval_for=<child>` + a direct `blocks` edge) that a human has closed
    ///   (`bd gate resolve`, the explicit bootstrap path).
    ///
    /// Consequently a `staged` child is consumable only when it is approved *and*
    /// its associated gate is resolved *and* no prerequisite remains open. A
    /// `staged` child that is never validated, rejected, gate-orphaned
    /// ([`ApprovalState::Missing`]), ambiguously gated
    /// ([`ApprovalState::Conflicting`]), or malformed-gated stays hidden — so an
    /// accidental `bd update --status open`, a status/date/label edit, or a
    /// missing/duplicated approval gate can never release unapproved work.
    /// Ordinary non-staged open work needs no gate; a genuine `--defer`
    /// postponement still hides any issue (date clear is not authorization).
    fn is_ready_candidate(&self, gates: &[ApprovalGate]) -> bool {
        // Never execute an epic or a gate issue, even when a CLI default filter
        // changes: the type check is explicit and independent of `--exclude-type`.
        if self.status != "open"
            || matches!(self.issue_type.as_deref(), Some("epic") | Some("gate"))
        {
            return false;
        }
        // A set defer date is a real postponement, not authorization. `undefer`
        // only makes a postponed task schedulable; it never approves staged work.
        if self.defer_until.is_some() {
            return false;
        }
        if self.has_label(PARKED_LABEL)
            || self.has_label(HUMAN_LABEL)
            || self.has_label(DECOMP_FIX_LABEL)
            || self.has_label(DRIFT_FIX_LABEL)
        {
            return false;
        }
        // A gate issue itself is never ordinary fleet work.
        if self.has_label(EXECUTION_APPROVAL_LABEL) {
            return false;
        }
        // A rejected child never executes, staged or not.
        if self.has_label(DECOMP_REJECTED_LABEL) {
            return false;
        }
        // A staged child additionally needs validator approval AND a resolved
        // native human gate association; anything else (missing/conflicting/
        // malformed/unresolved) fails closed.
        if self.has_label(STAGED_LABEL) {
            if !self.has_label(DECOMP_APPROVED_LABEL) {
                return false;
            }
            if approval_state(&self.id, &self.blocks_targets(), gates) != ApprovalState::Resolved {
                return false;
            }
        }
        true
    }

    /// Ids this issue is blocked by (`blocks` edge targets). The parent-child
    /// edge is ignored.
    fn blocks_targets(&self) -> Vec<String> {
        self.dependencies
            .as_deref()
            .unwrap_or_default()
            .iter()
            .filter(|edge| edge.edge_kind() == Some("blocks"))
            .filter_map(|edge| edge.target().map(str::to_string))
            .collect()
    }

    /// Project an issue into an approval-gate record. `approval_for` is read from
    /// the `approval_for` metadata key; `resolved` is true once the gate is
    /// closed. The native gate type/await fields are preserved so the association
    /// check can reject a non-native (task/timer) issue that merely copied the
    /// labels and metadata.
    fn into_gate(self) -> ApprovalGate {
        let approval_for = self
            .metadata
            .as_ref()
            .and_then(|meta| meta.get(APPROVAL_FOR_KEY))
            .and_then(Value::as_str)
            .map(str::to_string);
        ApprovalGate {
            id: self.id,
            approval_for,
            labels: self.labels.unwrap_or_default(),
            issue_type: self.issue_type,
            await_type: self.await_type,
            resolved: self.status == "closed",
        }
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

/// The epic's immutable review source derived from its live issue record.
fn source_from_issue(issue: &BdIssue) -> Result<ReviewSource, ContractError> {
    let design_text = issue
        .design
        .clone()
        .unwrap_or_else(|| issue.description.clone());
    let design = PackageArtifactRef::new(
        format!("bd:{}:design", issue.id),
        design_text.as_bytes(),
        "text/plain",
    );
    let definition_canonical = admission_definition_canonical(
        &issue.id,
        &issue.title,
        &issue.description,
        issue.design.as_deref(),
        issue.acceptance_criteria.as_deref(),
    )?;
    let definition_digest = admission_definition_digest(
        &issue.id,
        &issue.title,
        &issue.description,
        issue.design.as_deref(),
        issue.acceptance_criteria.as_deref(),
    )?;
    Ok(ReviewSource {
        definition_digest,
        definition_canonical,
        design,
        design_text,
    })
}

/// Current Unix time in milliseconds (0 on a pre-epoch clock; never panics).
fn now_unix_ms() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_millis() as u64)
        .unwrap_or(0)
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

/// Parse a bd payload that is either an array of objects or a single object,
/// returned as raw JSON values (preserving absent-vs-null field distinctions).
fn parse_value_array(stdout: &str) -> Result<Vec<Value>, ContractError> {
    let trimmed = stdout.trim();
    if trimmed.is_empty() {
        return Ok(Vec::new());
    }
    if let Ok(values) = serde_json::from_str::<Vec<Value>>(trimmed) {
        return Ok(values);
    }
    let one: Value = serde_json::from_str(trimmed)
        .map_err(|e| ContractError::KnownFailure(format!("cannot parse bd JSON: {e}")))?;
    Ok(vec![one])
}

/// Parse the first object of a bd payload (array or single), preserving raw
/// field presence.
fn parse_first_value(stdout: &str) -> Result<Value, ContractError> {
    parse_value_array(stdout)?
        .into_iter()
        .next()
        .ok_or_else(|| ContractError::KnownFailure("bd returned no issue".to_string()))
}

/// Whether a `bd show` payload carries a complete dependency projection.
///
/// Native bd reports `dependency_count=0` and omits the `dependencies` array for
/// a known-empty graph (accepted). A missing count, a non-numeric count, a
/// count>0 with an omitted/null array, a non-array value, or an array whose
/// length does not match the count is an incomplete/unknown graph and denies.
fn dependency_projection_complete(value: &Value) -> bool {
    let Some(count) = value.get("dependency_count").and_then(Value::as_u64) else {
        return false;
    };
    match value.get("dependencies") {
        None | Some(Value::Null) => count == 0,
        Some(Value::Array(items)) => items.len() as u64 == count,
        Some(_) => false,
    }
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
            await_type: None,
            status: "open".to_string(),
            assignee: None,
            started_at: None,
            defer_until: None,
            labels: None,
            metadata: None,
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

    /// An issue carrying `labels` and `blocks` edges to `blockers`.
    fn blocked(labels: &[&str], blockers: &[&str]) -> BdIssue {
        let dependencies = blockers
            .iter()
            .map(|b| BdDependency {
                depends_on_id: Some(b.to_string()),
                id: None,
                kind: Some("blocks".to_string()),
                dependency_type: None,
                status: None,
            })
            .collect();
        BdIssue {
            labels: Some(labels.iter().map(|l| l.to_string()).collect()),
            dependencies: Some(dependencies),
            ..issue()
        }
    }

    /// A well-formed native human gate authorizing `for_`.
    fn native_gate(id: &str, for_: &str, resolved: bool) -> ApprovalGate {
        ApprovalGate {
            id: id.to_string(),
            approval_for: Some(for_.to_string()),
            labels: vec![
                HUMAN_LABEL.to_string(),
                EXECUTION_APPROVAL_LABEL.to_string(),
            ],
            issue_type: Some("gate".to_string()),
            await_type: Some("human".to_string()),
            resolved,
        }
    }

    #[test]
    fn ordinary_open_undeferred_work_is_ready() {
        assert!(issue().is_ready_candidate(&[]));
        assert!(with_labels(&["difficulty:low"]).is_ready_candidate(&[]));
    }

    #[test]
    fn deferred_rejected_and_special_labels_stay_hidden() {
        let mut deferred = issue();
        deferred.defer_until = Some("2099-01-01T00:00:00Z".to_string());
        assert!(
            !deferred.is_ready_candidate(&[]),
            "a set defer date is a real postponement, not authorization"
        );

        let mut epic = issue();
        epic.issue_type = Some("epic".to_string());
        assert!(!epic.is_ready_candidate(&[]));

        for label in [
            PARKED_LABEL,
            HUMAN_LABEL,
            DECOMP_FIX_LABEL,
            DRIFT_FIX_LABEL,
            EXECUTION_APPROVAL_LABEL,
            DECOMP_REJECTED_LABEL,
        ] {
            assert!(
                !with_labels(&[label]).is_ready_candidate(&[]),
                "{label} must stay hidden"
            );
        }
    }

    #[test]
    fn staged_child_needs_validator_approval_and_a_resolved_native_gate() {
        // Never validated: hidden even with a resolved gate.
        assert!(
            !blocked(&[STAGED_LABEL], &["g"]).is_ready_candidate(&[native_gate("g", "x", true)])
        );
        // Approved but gate-orphaned (no association): hidden.
        assert!(!with_labels(&[STAGED_LABEL, DECOMP_APPROVED_LABEL]).is_ready_candidate(&[]));
        // Approved, gate present but unresolved: hidden.
        assert!(!blocked(&[STAGED_LABEL, DECOMP_APPROVED_LABEL], &["g"])
            .is_ready_candidate(&[native_gate("g", "x", false)]));
        // Approved + exactly one resolved native gate: ready.
        assert!(blocked(&[STAGED_LABEL, DECOMP_APPROVED_LABEL], &["g"])
            .is_ready_candidate(&[native_gate("g", "x", true)]));
        // Rejected alongside approval: still hidden.
        assert!(!blocked(
            &[STAGED_LABEL, DECOMP_APPROVED_LABEL, DECOMP_REJECTED_LABEL],
            &["g"]
        )
        .is_ready_candidate(&[native_gate("g", "x", true)]));
    }

    #[test]
    fn conflicting_and_malformed_gate_associations_deny() {
        let approved = || blocked(&[STAGED_LABEL, DECOMP_APPROVED_LABEL], &["g1", "g2"]);

        // Two associated gates: ambiguous ⇒ hidden.
        assert!(!approved()
            .is_ready_candidate(&[native_gate("g1", "x", true), native_gate("g2", "x", true),]));

        // approval_for names a different task: malformed ⇒ hidden.
        let mut wrong_target = blocked(&[STAGED_LABEL, DECOMP_APPROVED_LABEL], &["g"]);
        wrong_target.id = "x".to_string();
        assert!(!wrong_target.is_ready_candidate(&[native_gate("g", "other", true)]));

        // A plain task that copied the labels/metadata is not a native gate.
        let mut impostor = native_gate("g", "x", true);
        impostor.issue_type = Some("task".to_string());
        assert!(!wrong_target.is_ready_candidate(&[impostor]));

        // A timer/automation gate is not a native human gate.
        let mut timer = native_gate("g", "x", true);
        timer.await_type = Some("timer".to_string());
        assert!(!wrong_target.is_ready_candidate(&[timer]));

        // Missing labels on the gate: malformed ⇒ hidden.
        let mut unlabeled = native_gate("g", "x", true);
        unlabeled.labels = vec![HUMAN_LABEL.to_string()];
        assert!(!wrong_target.is_ready_candidate(&[unlabeled]));
    }

    #[test]
    fn status_and_date_edits_cannot_release_staged_work() {
        // `bd update --status open` flips the status but not the gate/approval:
        // a never-validated staged child stays invisible.
        let mut never_validated = with_labels(&[STAGED_LABEL]);
        never_validated.status = "open".to_string();
        assert!(!never_validated.is_ready_candidate(&[]));

        // A resolved gate plus a still-set (real) defer date stays hidden.
        let mut postponed = blocked(&[STAGED_LABEL, DECOMP_APPROVED_LABEL], &["g"]);
        postponed.defer_until = Some("2099-01-01T00:00:00Z".to_string());
        assert!(!postponed.is_ready_candidate(&[native_gate("g", "x", true)]));
    }

    #[test]
    fn approval_state_classifies_every_association() {
        assert_eq!(approval_state("x", &[], &[]), ApprovalState::Missing);
        assert_eq!(
            approval_state("x", &["g".into()], &[native_gate("g", "x", false)]),
            ApprovalState::Unresolved
        );
        assert_eq!(
            approval_state("x", &["g".into()], &[native_gate("g", "x", true)]),
            ApprovalState::Resolved
        );
        assert_eq!(
            approval_state(
                "x",
                &["g1".into(), "g2".into()],
                &[native_gate("g1", "x", true), native_gate("g2", "x", true)]
            ),
            ApprovalState::Conflicting
        );
        assert_eq!(
            approval_state("x", &["g".into()], &[native_gate("g", "other", true)]),
            ApprovalState::Malformed
        );
    }

    #[test]
    fn unrelated_native_gate_is_not_an_approval_association() {
        // A native prerequisite gate with neither the `execution-approval` label
        // nor `approval_for` claims no approval: it is ignored here (bd's own
        // blocker semantics govern it), so a valid resolved approval gate still
        // releases the child.
        let mut unrelated = native_gate("dep", "x", false);
        unrelated.labels = Vec::new();
        unrelated.approval_for = None;
        let child = blocked(&[STAGED_LABEL, DECOMP_APPROVED_LABEL], &["g", "dep"]);
        assert_eq!(
            approval_state(
                "x",
                &child.blocks_targets(),
                &[native_gate("g", "x", true), unrelated]
            ),
            ApprovalState::Resolved,
            "an unrelated native prerequisite gate is not an approval association"
        );
        assert!(child.is_ready_candidate(&[native_gate("g", "x", true)]));
    }

    #[test]
    fn gate_and_epic_type_issues_are_never_ready() {
        // A native gate carrying an intake block and no exclusion label must
        // never be executable: the type check rejects it explicitly.
        let mut gate = issue();
        gate.issue_type = Some("gate".to_string());
        gate.description = "Intro.\n\n```yaml\ndirective: directive/v0\n```\n".to_string();
        assert!(
            !gate.is_ready_candidate(&[]),
            "a gate issue is never ordinary work"
        );
        let mut epic = issue();
        epic.issue_type = Some("epic".to_string());
        assert!(!epic.is_ready_candidate(&[]));
    }

    /// A fake `bd` whose union queries return `first` and `second` respectively.
    fn fake_gate_bd(dir: &agalma_testkit::ScratchDir, first: &str, second: &str) -> String {
        use std::os::unix::fs::PermissionsExt;
        let a = dir.join("first.json");
        let b = dir.join("second.json");
        std::fs::write(&a, first).expect("write first");
        std::fs::write(&b, second).expect("write second");
        let script = dir.join("fakebd.sh");
        std::fs::write(
            &script,
            format!(
                "#!/bin/sh\ncase \"$*\" in\n  *--type*) cat {a} ;;\n  *) cat {b} ;;\nesac\n",
                a = a.display(),
                b = b.display(),
            ),
        )
        .expect("write fake bd");
        std::fs::set_permissions(&script, std::fs::Permissions::from_mode(0o755)).expect("chmod");
        script.to_string_lossy().into_owned()
    }

    #[test]
    fn approval_gates_keeps_differing_observations_as_a_conflict() {
        // The same gate id observed closed in `--type gate`, reopened in
        // `--label execution-approval`: the union must keep both so the exact-one
        // rule fails closed instead of retaining a stale closed record.
        let mut dir = agalma_testkit::ScratchDir::new("bd-fake-gate-union");
        let closed = r#"[{"id":"g1","title":"Gate","status":"closed","issue_type":"gate","await_type":"human","labels":["human","execution-approval"],"metadata":{"approval_for":"c1"}}]"#;
        let reopened = r#"[{"id":"g1","title":"Gate","status":"open","issue_type":"gate","await_type":"human","labels":["human","execution-approval"],"metadata":{"approval_for":"c1"}}]"#;
        let program = fake_gate_bd(&dir, closed, reopened);

        let queue = BdTaskQueue::new(dir.path()).with_program(program);
        let gates = queue.approval_gates().expect("fake gates");
        assert_eq!(
            gates.len(),
            2,
            "differing observations for one identity are both kept: {gates:?}"
        );
        assert_eq!(
            approval_state("c1", &["g1".to_string()], &gates),
            ApprovalState::Conflicting,
            "conflicting observations of one gate identity must fail closed"
        );
        dir.keep();
    }

    #[test]
    fn approval_gates_normalizes_label_order_across_queries() {
        // The same native gate returned with reordered labels is one observation,
        // not a spurious conflict.
        let mut dir = agalma_testkit::ScratchDir::new("bd-fake-gate-label-order");
        let first = r#"[{"id":"g1","title":"Gate","status":"closed","issue_type":"gate","await_type":"human","labels":["human","execution-approval"],"metadata":{"approval_for":"c1"}}]"#;
        let second = r#"[{"id":"g1","title":"Gate","status":"closed","issue_type":"gate","await_type":"human","labels":["execution-approval","human"],"metadata":{"approval_for":"c1"}}]"#;
        let program = fake_gate_bd(&dir, first, second);

        let queue = BdTaskQueue::new(dir.path()).with_program(program);
        let gates = queue.approval_gates().expect("fake gates");
        assert_eq!(
            gates.len(),
            1,
            "reordered labels are the same observation: {gates:?}"
        );
        assert_eq!(
            approval_state("c1", &["g1".to_string()], &gates),
            ApprovalState::Resolved
        );
        dir.keep();
    }

    // -----------------------------------------------------------------------
    // Property test: readiness is exactly determined by the generated spec.
    //
    // The expected release result is computed from the generated specification
    // inputs (approval/rejection, real postponement, gate count, native type,
    // target association, labels, resolution) — never from the production
    // `approval_state`. This keeps the oracle independent, so a classifier bug
    // (e.g. denying every staged task) fails the exact-equivalence assertion
    // instead of satisfying a safety-only one. Deterministic, bounded, and
    // failure-replayable (seed + case index + inputs).
    // -----------------------------------------------------------------------

    /// A tiny deterministic generator (no external dependency): reproducible
    /// operation sequences under a fixed seed.
    struct Lcg(u64);
    impl Lcg {
        fn next(&mut self) -> u64 {
            self.0 = self
                .0
                .wrapping_mul(6364136223846793005)
                .wrapping_add(1442695040888963407);
            self.0 >> 33
        }
        fn below(&mut self, n: u64) -> u64 {
            self.next() % n
        }
    }

    /// One generated scenario. Every field is an independent input; the expected
    /// readiness is derived from them, not from the code under test.
    #[derive(Clone, Copy, Debug)]
    struct Spec {
        staged: bool,
        approved: bool,
        rejected: bool,
        deferred: bool,
        /// Number of `blocks` edges to candidate gate issues (0, 1, or 2).
        gate_count: usize,
        /// Gate is a native human gate (`issue_type=gate`, `await_type=human`).
        native_type: bool,
        /// Gate labels carry both `human` and `execution-approval`.
        labelled: bool,
        /// Gate `approval_for` names this child.
        correct_target: bool,
        /// Gate is resolved (closed).
        resolved: bool,
        /// Also attach an unrelated native prerequisite gate (no
        /// `execution-approval` label, no `approval_for`); it is an ordinary
        /// dependency blocker, never an approval association.
        unrelated_gate: bool,
    }

    impl Spec {
        /// The independently derived readiness: a staged child releases only when
        /// approved, unrejected, undeferred, and pinned by exactly one resolved,
        /// native, correctly-labelled, correctly-targeted approval gate. An
        /// unrelated native prerequisite gate never participates in approval.
        fn expected_ready(&self) -> bool {
            if self.rejected || self.deferred {
                return false;
            }
            if !self.staged {
                return true;
            }
            self.approved
                && self.gate_count == 1
                && self.native_type
                && self.labelled
                && self.correct_target
                && self.resolved
        }

        fn build(&self) -> (BdIssue, Vec<ApprovalGate>) {
            let mut labels = Vec::new();
            if self.staged {
                labels.push(STAGED_LABEL.to_string());
            }
            if self.approved {
                labels.push(DECOMP_APPROVED_LABEL.to_string());
            }
            if self.rejected {
                labels.push(DECOMP_REJECTED_LABEL.to_string());
            }

            let mut blockers = Vec::new();
            let mut gates = Vec::new();
            for index in 0..self.gate_count {
                let gate_id = format!("gate-{index}");
                blockers.push(gate_id.clone());
                let mut gate_labels = Vec::new();
                if self.labelled {
                    gate_labels.push(HUMAN_LABEL.to_string());
                    gate_labels.push(EXECUTION_APPROVAL_LABEL.to_string());
                }
                gates.push(ApprovalGate {
                    id: gate_id,
                    approval_for: Some(if self.correct_target {
                        "x".to_string()
                    } else {
                        "someone-else".to_string()
                    }),
                    labels: gate_labels,
                    issue_type: Some(if self.native_type { "gate" } else { "task" }.to_string()),
                    await_type: Some(if self.native_type { "human" } else { "timer" }.to_string()),
                    resolved: self.resolved,
                });
            }
            if self.unrelated_gate {
                // A native prerequisite gate that claims no execution approval.
                blockers.push("unrelated".to_string());
                gates.push(ApprovalGate {
                    id: "unrelated".to_string(),
                    approval_for: None,
                    labels: Vec::new(),
                    issue_type: Some("gate".to_string()),
                    await_type: Some("human".to_string()),
                    resolved: self.resolved,
                });
            }

            let mut issue = blocked(
                &labels.iter().map(String::as_str).collect::<Vec<_>>(),
                &blockers.iter().map(String::as_str).collect::<Vec<_>>(),
            );
            if self.deferred {
                issue.defer_until = Some("2099-01-01T00:00:00Z".to_string());
            }
            (issue, gates)
        }
    }

    #[test]
    fn ready_predicate_is_exactly_determined_by_the_generated_spec() {
        // Fixed seed + bounded budget so any failure is replayable.
        const SEED: u64 = 0x5eed_1234_abcd_0001;
        const CASES: usize = 6000;
        let mut rng = Lcg(SEED);

        for case in 0..CASES {
            let spec = Spec {
                staged: rng.below(2) == 0,
                approved: rng.below(2) == 0,
                rejected: rng.below(4) == 0,
                deferred: rng.below(3) == 0,
                gate_count: match rng.below(6) {
                    0 => 0,
                    1..=3 => 1,
                    _ => 2,
                },
                unrelated_gate: rng.below(3) == 0,
                native_type: rng.below(8) != 0,
                labelled: rng.below(8) != 0,
                correct_target: rng.below(8) != 0,
                resolved: rng.below(2) == 0,
            };
            let (issue, gates) = spec.build();
            let ready = issue.is_ready_candidate(&gates);
            assert_eq!(
                ready,
                spec.expected_ready(),
                "seed={SEED:#x} case={case} spec={spec:?} (replay: rerun with the same seed)"
            );
            // Determinism: a pure predicate must return the same result twice.
            assert_eq!(
                ready,
                issue.is_ready_candidate(&gates),
                "idempotent case={case}"
            );
        }
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
        assert_eq!(
            definition.acceptance_criteria.as_deref(),
            Some("Acceptance text.")
        );
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
