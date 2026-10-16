//! `TaskQueueApi` — task backlog intake, claims, and status projection.
//!
//! A task is a durable backlog record (initially a bd issue) plus a
//! machine-readable fenced YAML block in its body. The block is parsed by the
//! adapter; this module owns only the canonical record and the trait seam.
//!
//! The queue adapter (`agalma-taskqueue`) hides the bd CLI: issue bodies, JSON
//! shapes, and native ids never leak through this contract. Identity is the bd
//! issue id ([`TaskId`]); execution identity is derived from it elsewhere.
//!
//! Claim semantics are atomic: `claim` performs the adapter's compare-and-set
//! (bd `--claim`) and returns [`ClaimEvidence`] describing the persisted state;
//! the caller records a fenced lease before dispatch. Status projection
//! (`comment`, `close_task`, `add_label`) is idempotent and reconciliable from
//! durable ledger state.

use serde::{Deserialize, Serialize};

use crate::digest::canonical_digest;
use crate::error::ContractError;
use crate::ids::TaskId;
use crate::task_package::PackageArtifactRef;

/// Task family (allowed set, architecture §5).
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum TaskFamily {
    Bench,
    Feature,
    Fix,
    Mutation,
}

impl TaskFamily {
    /// Every allowed family.
    pub const ALLOWED: [TaskFamily; 4] = [
        TaskFamily::Bench,
        TaskFamily::Feature,
        TaskFamily::Fix,
        TaskFamily::Mutation,
    ];

    /// Parse the wire form; `None` when the value is outside the allowed set.
    pub fn parse(raw: &str) -> Option<Self> {
        match raw {
            "bench" => Some(TaskFamily::Bench),
            "feature" => Some(TaskFamily::Feature),
            "fix" => Some(TaskFamily::Fix),
            "mutation" => Some(TaskFamily::Mutation),
            _ => None,
        }
    }

    /// The wire form.
    pub fn as_str(self) -> &'static str {
        match self {
            TaskFamily::Bench => "bench",
            TaskFamily::Feature => "feature",
            TaskFamily::Fix => "fix",
            TaskFamily::Mutation => "mutation",
        }
    }
}

/// Priority class within the ready set (low → critical).
///
/// Ordering is declaration order, so `max` picks the highest-priority ready
/// task (the `triage.pick-next` static policy is priority, then age, then id).
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum TaskPriority {
    Low,
    Normal,
    High,
    Critical,
}

impl TaskPriority {
    /// Every allowed priority.
    pub const ALLOWED: [TaskPriority; 4] = [
        TaskPriority::Low,
        TaskPriority::Normal,
        TaskPriority::High,
        TaskPriority::Critical,
    ];

    /// Parse the wire form; `None` when the value is outside the allowed set.
    pub fn parse(raw: &str) -> Option<Self> {
        match raw {
            "low" => Some(TaskPriority::Low),
            "normal" => Some(TaskPriority::Normal),
            "high" => Some(TaskPriority::High),
            "critical" => Some(TaskPriority::Critical),
            _ => None,
        }
    }

    /// The wire form.
    pub fn as_str(self) -> &'static str {
        match self {
            TaskPriority::Low => "low",
            TaskPriority::Normal => "normal",
            TaskPriority::High => "high",
            TaskPriority::Critical => "critical",
        }
    }
}

/// Canonical task record: issue identity plus the parsed fenced block.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct Task {
    /// Backlog identity (bd issue id).
    pub task_id: TaskId,
    /// Human title (issue title).
    pub title: String,
    /// Directive version that authorized the task (`directive/vN`).
    pub directive: String,
    /// Target repository (`agalma`).
    pub target: String,
    /// Base ref the candidate branches from (`main`, `genome/v7`, ...).
    pub base_ref: String,
    /// Acceptance commands that must pass; run by Rust, never the model.
    pub acceptance: Vec<String>,
    /// Optional spend cap in USD for the task.
    pub budget_usd: Option<f64>,
    /// Priority class.
    pub priority: TaskPriority,
    /// Task family.
    pub family: TaskFamily,
    /// Bounded build/verify attempts.
    pub max_attempts: u32,
    /// The raw fenced block body exactly as authored (without the fence lines).
    pub raw_block: String,
}

/// Why a ready issue was not eligible as a task.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum SkipReason {
    /// The description has no fenced task block; the issue is not a task.
    MissingBlock,
}

/// A ready issue skipped during intake, with the reason.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct SkippedIssue {
    pub task_id: TaskId,
    pub reason: SkipReason,
}

/// Intake result: parsed ready tasks plus the reconcile report of skipped
/// issues. A skipped issue is not an error; malformed blocks are.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct ReadyTasks {
    pub tasks: Vec<Task>,
    pub skipped: Vec<SkippedIssue>,
}

/// Evidence that an atomic claim was persisted.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct ClaimEvidence {
    pub task_id: TaskId,
    /// Backlog status after the claim (adapter-native, e.g. `in_progress`).
    pub status: String,
    /// Claimant, when the backend records one.
    pub assignee: Option<String>,
    /// Claim start timestamp, when the backend records one.
    pub started_at: Option<String>,
}

// ---------------------------------------------------------------------------
// Prerequisite-aware epic review admission (`EpicReviewAdmission/v1`)
// ---------------------------------------------------------------------------
//
// SF.11. The typed projection the queue adapter computes so the conductor can
// fail-closed guard paid epic review and closure with the actual blocking
// prerequisite graph. `docs/self-mutation-contracts.md` §"Prerequisite-aware
// epic review"; schema `EpicReviewAdmission` and example
// `epic-review-admission.json`.

/// The `EpicReviewAdmission/v1` wire format identifier.
pub const EPIC_REVIEW_ADMISSION_FORMAT: &str = "EpicReviewAdmission/v1";
/// The negotiated capability that enables automated review admission. An
/// implementation that does not advertise it refuses automated review rather
/// than bypassing admission.
pub const REVIEW_ADMISSION_CAPABILITY: &str = "review-admission/v1";

/// The kind of a direct edge from the epic to another issue.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum AdmissionEdgeType {
    /// A true blocking prerequisite: an open target denies review.
    Blocks,
    /// A non-blocking tracking edge: ignored by the admission decision.
    Tracks,
    /// The parent-child edge: ignored by the admission decision.
    ParentChild,
}

impl AdmissionEdgeType {
    /// The schema/wire string.
    pub const fn as_str(self) -> &'static str {
        match self {
            AdmissionEdgeType::Blocks => "blocks",
            AdmissionEdgeType::Tracks => "tracks",
            AdmissionEdgeType::ParentChild => "parent-child",
        }
    }

    /// Parse the wire form; `None` for an unknown edge kind (a malformed graph).
    pub fn parse(raw: &str) -> Option<Self> {
        match raw {
            "blocks" => Some(AdmissionEdgeType::Blocks),
            "tracks" => Some(AdmissionEdgeType::Tracks),
            "parent-child" => Some(AdmissionEdgeType::ParentChild),
            _ => None,
        }
    }
}

/// One implementation child in the admitted projection.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct AdmissionChild {
    /// Child identity.
    pub id: String,
    /// Immutable source-definition digest of the child.
    pub definition_digest: String,
    /// Observed child status (`closed`, `open`, ...).
    pub status: String,
    /// Child labels (normalized by UTF-8 order).
    pub labels: Vec<String>,
}

/// A direct edge from the epic to a target issue.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct AdmissionEdge {
    /// The other issue's id.
    pub target_id: String,
    /// Edge kind.
    #[serde(rename = "type")]
    pub edge_type: AdmissionEdgeType,
    /// Observed target status, or `null` when the target/queue data is unknown.
    pub observed_status: Option<String>,
}

/// The deterministic admission decision.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ReviewAdmissionDecision {
    /// Every child is closed, at least one is staged, and no blocking
    /// prerequisite is open: paid review may proceed.
    Eligible,
    /// An open `blocks` prerequisite: wait, do not dispatch or mutate.
    WaitingPrerequisite,
    /// A child is not closed (or no staged child exists): not a review candidate.
    IncompleteChildren,
    /// A zero-child epic is never a review candidate.
    IneligibleEmpty,
    /// Missing/malformed/unknown graph data or unavailable queue state: deny.
    Unavailable,
    /// The supplied immutable context no longer matches the live source: deny.
    Stale,
}

impl ReviewAdmissionDecision {
    /// Whether paid review may proceed.
    pub fn is_eligible(self) -> bool {
        matches!(self, ReviewAdmissionDecision::Eligible)
    }

    /// Whether an open blocking prerequisite requires waiting (no paid review,
    /// no drift mutation, no global drift hold).
    pub fn is_waiting_prerequisite(self) -> bool {
        matches!(self, ReviewAdmissionDecision::WaitingPrerequisite)
    }
}

/// The immutable review context the caller supplies alongside the queue read.
///
/// The caller supplies only the admitted immutable inputs — the design artifact
/// reference and the base/candidate commits — never child status or an
/// eligibility override.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ReviewAdmissionContext {
    /// The immutable epic design artifact reference.
    pub design: PackageArtifactRef,
    /// The review diff baseline commit.
    pub base_sha: String,
    /// The integrated candidate commit under review.
    pub candidate_sha: String,
}

/// The canonical prerequisite-aware epic review admission projection.
///
/// `input_digest` is the canonical-json/v1 + SHA-256 identity of the complete
/// record excluding `input_digest` itself (the `canonical-json/v1` rules in
/// `crate::digest`). Integrity is verified by recomputation; two fresh reads of
/// unchanged inputs differ only in `observed_at_unix_ms` and `input_digest`, so
/// revalidation compares [`semantic_inputs_equal`](Self::semantic_inputs_equal)
/// rather than the full-record digest.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct EpicReviewAdmission {
    /// Always `EpicReviewAdmission/v1`.
    pub format: String,
    /// Epic identity.
    pub epic_id: String,
    /// Immutable source-definition digest of the epic.
    pub definition_digest: String,
    /// The immutable design artifact reference (echoed from the context).
    pub design: PackageArtifactRef,
    /// The review diff baseline commit.
    pub base_sha: String,
    /// The integrated candidate commit under review.
    pub candidate_sha: String,
    /// The complete child set, normalized by id.
    pub children: Vec<AdmissionChild>,
    /// The epic's direct edges, normalized by (type, target).
    pub edges: Vec<AdmissionEdge>,
    /// The deterministic decision.
    pub decision: ReviewAdmissionDecision,
    /// The open `blocks` prerequisite target ids (sorted).
    pub blocking_ids: Vec<String>,
    /// Observation time (Unix milliseconds).
    pub observed_at_unix_ms: u64,
    /// The canonical full-record identity.
    pub input_digest: String,
}

impl EpicReviewAdmission {
    /// Recompute the canonical `input_digest` over the record excluding itself.
    pub fn compute_input_digest(&self) -> Result<String, ContractError> {
        let mut value = serde_json::to_value(self).map_err(|e| {
            ContractError::KnownFailure(format!("EpicReviewAdmission: cannot encode: {e}"))
        })?;
        if let Some(map) = value.as_object_mut() {
            map.remove("input_digest");
        }
        canonical_digest(&value)
    }

    /// Verify the recorded `input_digest` matches the record's own contents.
    pub fn verify_integrity(&self) -> Result<(), ContractError> {
        let expected = self.compute_input_digest()?;
        if expected == self.input_digest {
            Ok(())
        } else {
            Err(ContractError::KnownFailure(format!(
                "EpicReviewAdmission {}: input_digest mismatch",
                self.epic_id
            )))
        }
    }

    /// Whether two records carry equivalent semantic review inputs.
    ///
    /// Compares `epic_id`, `definition_digest`, the immutable `design`, both
    /// SHAs, the complete children, and the direct edges. Children are compared
    /// by id and edges by (type, target); label sets are normalized by UTF-8
    /// order. Observation time and the full-record `input_digest` are excluded:
    /// a fresh observation or an equivalent row order alone never invalidates a
    /// review.
    pub fn semantic_inputs_equal(&self, other: &Self) -> bool {
        self.epic_id == other.epic_id
            && self.definition_digest == other.definition_digest
            && self.design == other.design
            && self.base_sha == other.base_sha
            && self.candidate_sha == other.candidate_sha
            && normalize_children(&self.children) == normalize_children(&other.children)
            && normalize_edges(&self.edges) == normalize_edges(&other.edges)
    }
}

/// Normalize children by id; labels sorted by UTF-8 byte order.
pub fn normalize_children(children: &[AdmissionChild]) -> Vec<AdmissionChild> {
    let mut normalized: Vec<AdmissionChild> = children
        .iter()
        .map(|child| {
            let mut labels = child.labels.clone();
            labels.sort();
            AdmissionChild {
                id: child.id.clone(),
                definition_digest: child.definition_digest.clone(),
                status: child.status.clone(),
                labels,
            }
        })
        .collect();
    normalized.sort_by(|a, b| a.id.cmp(&b.id));
    normalized
}

/// Normalize edges by (type, target).
pub fn normalize_edges(edges: &[AdmissionEdge]) -> Vec<AdmissionEdge> {
    let mut normalized = edges.to_vec();
    normalized.sort_by(|a, b| {
        (a.edge_type.as_str(), a.target_id.as_str())
            .cmp(&(b.edge_type.as_str(), b.target_id.as_str()))
    });
    normalized
}

/// The derived blocking prerequisite ids: every `blocks` edge whose observed
/// target status is not exactly `closed`, sorted and deduplicated.
pub fn blocking_prerequisite_ids(edges: &[AdmissionEdge]) -> Vec<String> {
    let mut ids: Vec<String> = edges
        .iter()
        .filter(|edge| edge.edge_type == AdmissionEdgeType::Blocks)
        .filter(|edge| edge.observed_status.as_deref() != Some("closed"))
        .map(|edge| edge.target_id.clone())
        .collect();
    ids.sort();
    ids.dedup();
    ids
}

/// The canonical immutable source-definition digest of an issue.
///
/// Covers the identity and immutable design/acceptance text (title, body,
/// design field, acceptance criteria) — never notes or comments, which carry
/// mutable handoff narration. Deterministic across equivalent reads.
pub fn admission_definition_digest(
    id: &str,
    title: &str,
    description: &str,
    design: Option<&str>,
    acceptance_criteria: Option<&str>,
) -> Result<String, ContractError> {
    let canonical =
        admission_definition_canonical(id, title, description, design, acceptance_criteria)?;
    Ok(crate::digest::sha256_digest(canonical.as_bytes()))
}

/// The `staged` decomposition-child label (single-sourced value).
pub const ADMISSION_STAGED_LABEL: &str = "staged";

/// The deterministic admission decision for a known child/edge projection.
///
/// `malformed` is true when the caller detected structural invalidity (unknown/
/// duplicate/ambiguous rows, a missing projection, or a reachable cycle); it
/// denies before any other classification.
///
/// An `Eligible` decision requires a non-empty, fully-closed child set with at
/// least one staged child and no blocking prerequisite (the frozen
/// `validate.py` invariant). Every projected child is implementation work: there
/// is no open-child exemption.
pub fn decide_review_admission(
    children: &[AdmissionChild],
    edges: &[AdmissionEdge],
    malformed: bool,
) -> ReviewAdmissionDecision {
    if malformed {
        return ReviewAdmissionDecision::Unavailable;
    }
    if children.is_empty() {
        return ReviewAdmissionDecision::IneligibleEmpty;
    }
    if !blocking_prerequisite_ids(edges).is_empty() {
        return ReviewAdmissionDecision::WaitingPrerequisite;
    }
    let has_staged = children
        .iter()
        .any(|child| child.labels.iter().any(|l| l == ADMISSION_STAGED_LABEL));
    let all_children_closed = children.iter().all(|child| child.status == "closed");
    if !all_children_closed || !has_staged {
        return ReviewAdmissionDecision::IncompleteChildren;
    }
    ReviewAdmissionDecision::Eligible
}

/// The canonical immutable source-definition bytes of an issue.
///
/// Covers the identity and immutable design/acceptance text (title, body,
/// design field, acceptance criteria) — never notes or comments, which carry
/// mutable handoff narration. Deterministic across equivalent reads.
pub fn admission_definition_canonical(
    id: &str,
    title: &str,
    description: &str,
    design: Option<&str>,
    acceptance_criteria: Option<&str>,
) -> Result<String, ContractError> {
    let value = serde_json::json!({
        "id": id,
        "title": title,
        "description": description,
        "design": design,
        "acceptance_criteria": acceptance_criteria,
    });
    crate::digest::canonical_json(&value)
}

/// Task backlog contract. Issue bodies, CLI calls, and native ids stay private.
pub trait TaskQueueApi {
    /// Ready tasks: open, unclaimed, and not parked, with a reconcile report of
    /// non-task issues that were skipped (missing block).
    fn ready_tasks(&self) -> Result<ReadyTasks, ContractError>;

    /// Load one task by id. A missing/malformed block is an error here (a
    /// specific issue was requested, not scanned).
    fn get(&self, id: &TaskId) -> Result<Task, ContractError>;

    /// Atomically claim a task and return evidence of the persisted claim.
    fn claim(&mut self, id: &TaskId) -> Result<ClaimEvidence, ContractError>;

    /// Append a status-projection comment (e.g. `phase=… execution=… lease=…`).
    fn comment(&mut self, id: &TaskId, text: &str) -> Result<(), ContractError>;

    /// Close a task (terminal `done`) with a reason.
    fn close_task(&mut self, id: &TaskId, reason: &str) -> Result<(), ContractError>;

    /// Add a label (e.g. `parked`) to a task.
    fn add_label(&mut self, id: &TaskId, label: &str) -> Result<(), ContractError>;

    /// Compute the prerequisite-aware epic review admission projection (SF.11).
    ///
    /// Reads the complete current source/children/edge projection and returns a
    /// deterministic [`EpicReviewAdmission`]; the caller supplies the admitted
    /// immutable [`ReviewAdmissionContext`], never child status or an override.
    ///
    /// The default implementation denies automated review until an
    /// implementation negotiates [`REVIEW_ADMISSION_CAPABILITY`], so an old
    /// adapter can never silently bypass admission.
    fn review_admission(
        &self,
        epic_id: &str,
        context: &ReviewAdmissionContext,
    ) -> Result<EpicReviewAdmission, ContractError> {
        let _ = (epic_id, context);
        Err(ContractError::UnsupportedCapability(format!(
            "task queue does not support {REVIEW_ADMISSION_CAPABILITY}: automated review admission is denied"
        )))
    }
}

#[cfg(test)]
mod review_admission_tests {
    use super::*;

    fn design() -> PackageArtifactRef {
        PackageArtifactRef::new("bd:rev-e:design", b"epic design", "text/plain")
    }

    fn child(id: &str, status: &str, staged: bool) -> AdmissionChild {
        let labels = if staged {
            vec!["staged".to_string(), "difficulty:low".to_string()]
        } else {
            vec!["difficulty:low".to_string()]
        };
        AdmissionChild {
            id: id.to_string(),
            definition_digest: "sha256:".to_string() + &"0".repeat(64),
            status: status.to_string(),
            labels,
        }
    }

    #[test]
    fn definition_digest_is_deterministic_and_source_sensitive() {
        let a = admission_definition_digest("e", "T", "body", None, Some("acc")).unwrap();
        let b = admission_definition_digest("e", "T", "body", None, Some("acc")).unwrap();
        assert_eq!(a, b, "identical source → identical digest");
        assert!(a.starts_with("sha256:"), "{a}");

        // A design edit changes the digest; notes are not part of the digest.
        let edited =
            admission_definition_digest("e", "T", "body", Some("new"), Some("acc")).unwrap();
        assert_ne!(a, edited, "a design change must change the source digest");
    }

    #[test]
    fn blocking_ids_exclude_closed_and_non_blocking_edges() {
        let edges = vec![
            AdmissionEdge {
                target_id: "open-dep".to_string(),
                edge_type: AdmissionEdgeType::Blocks,
                observed_status: Some("open".to_string()),
            },
            AdmissionEdge {
                target_id: "unknown-dep".to_string(),
                edge_type: AdmissionEdgeType::Blocks,
                observed_status: None,
            },
            AdmissionEdge {
                target_id: "closed-dep".to_string(),
                edge_type: AdmissionEdgeType::Blocks,
                observed_status: Some("closed".to_string()),
            },
            AdmissionEdge {
                target_id: "tracked".to_string(),
                edge_type: AdmissionEdgeType::Tracks,
                observed_status: Some("open".to_string()),
            },
        ];
        assert_eq!(
            blocking_prerequisite_ids(&edges),
            vec!["open-dep".to_string(), "unknown-dep".to_string()],
            "only unresolved blocks edges gate, sorted"
        );
    }

    #[test]
    fn input_digest_round_trips_and_semantic_compare_ignores_order_and_time() {
        let mk = |observed_at: u64, reverse: bool| {
            let mut children = vec![child("c1", "closed", true), child("c2", "closed", false)];
            if reverse {
                children.reverse();
                for c in &mut children {
                    c.labels.reverse();
                }
            }
            let edges = vec![AdmissionEdge {
                target_id: "p".to_string(),
                edge_type: AdmissionEdgeType::Blocks,
                observed_status: Some("closed".to_string()),
            }];
            let mut record = EpicReviewAdmission {
                format: EPIC_REVIEW_ADMISSION_FORMAT.to_string(),
                epic_id: "rev-e".to_string(),
                definition_digest: "sha256:".to_string() + &"1".repeat(64),
                design: design(),
                base_sha: "a".repeat(40),
                candidate_sha: "b".repeat(40),
                children,
                edges,
                decision: ReviewAdmissionDecision::Eligible,
                blocking_ids: Vec::new(),
                observed_at_unix_ms: observed_at,
                input_digest: String::new(),
            };
            record.input_digest = record.compute_input_digest().unwrap();
            record
        };

        let left = mk(1, false);
        let right = mk(2, true);
        left.verify_integrity().unwrap();
        right.verify_integrity().unwrap();
        assert!(left.semantic_inputs_equal(&right));
        assert_ne!(left.input_digest, right.input_digest);

        // Tampering breaks integrity.
        let mut tampered = left.clone();
        tampered.base_sha = "c".repeat(40);
        assert!(tampered.verify_integrity().is_err());
    }

    #[test]
    fn default_review_admission_denies_unsupported() {
        struct Unsupported;
        impl TaskQueueApi for Unsupported {
            fn ready_tasks(&self) -> Result<ReadyTasks, ContractError> {
                unimplemented!()
            }
            fn get(&self, _id: &TaskId) -> Result<Task, ContractError> {
                unimplemented!()
            }
            fn claim(&mut self, _id: &TaskId) -> Result<ClaimEvidence, ContractError> {
                unimplemented!()
            }
            fn comment(&mut self, _id: &TaskId, _text: &str) -> Result<(), ContractError> {
                unimplemented!()
            }
            fn close_task(&mut self, _id: &TaskId, _reason: &str) -> Result<(), ContractError> {
                unimplemented!()
            }
            fn add_label(&mut self, _id: &TaskId, _label: &str) -> Result<(), ContractError> {
                unimplemented!()
            }
        }
        let context = ReviewAdmissionContext {
            design: design(),
            base_sha: "a".repeat(40),
            candidate_sha: "b".repeat(40),
        };
        let err = Unsupported.review_admission("rev-e", &context).unwrap_err();
        assert!(
            matches!(err, ContractError::UnsupportedCapability(_)),
            "{err:?}"
        );
    }
}
