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
use serde::Deserialize;

use crate::block::parse_task_block;

/// Label marking a task terminal-but-unfinished (design §"Task model").
pub const PARKED_LABEL: &str = "parked";

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
}

impl TaskQueueApi for BdTaskQueue {
    fn ready_tasks(&self) -> Result<ReadyTasks, ContractError> {
        let stdout = self.bd_ok(&[
            "list",
            "--json",
            "--status",
            "open",
            "--exclude-label",
            PARKED_LABEL,
            "--limit",
            "0",
        ])?;
        let issues = parse_issues(&stdout)?;

        let mut tasks = Vec::new();
        let mut skipped = Vec::new();
        for issue in issues {
            // Defensive re-check: bd filters above, but the projection must not
            // depend on the CLI's flag semantics.
            if issue.status != "open" || issue.has_label(PARKED_LABEL) {
                continue;
            }
            let id = issue.id.clone();
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

/// Subset of the bd issue JSON the adapter consumes.
#[derive(Debug, Deserialize)]
struct BdIssue {
    id: String,
    title: String,
    #[serde(default)]
    description: String,
    status: String,
    #[serde(default)]
    assignee: Option<String>,
    #[serde(default)]
    started_at: Option<String>,
    #[serde(default)]
    labels: Option<Vec<String>>,
}

impl BdIssue {
    fn has_label(&self, label: &str) -> bool {
        self.labels
            .as_deref()
            .is_some_and(|labels| labels.iter().any(|l| l == label))
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
