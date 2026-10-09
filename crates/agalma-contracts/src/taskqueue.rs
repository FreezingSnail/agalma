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

use crate::error::ContractError;
use crate::ids::TaskId;

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
}
