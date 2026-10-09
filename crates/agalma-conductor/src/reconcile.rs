//! Ledger-vs-projection reconcile (M1.6, `agalma-52k.7`).
//!
//! The ledger is the source of truth for execution progress; the bd issue is a
//! *projection* derived from it. A crash between a ledger transition and its
//! projection write leaves the two out of sync. [`reconcile_projection`]
//! compares each task's derived ledger state against its bd status, labels, and
//! phase comments, and rewrites the projection to match (never the ledger),
//! reporting a [`ReconcileSummary`].
//!
//! Rules (ledger wins):
//!
//! - terminal `done` execution → issue `closed`, `parked` label removed, and a
//!   `phase=done execution=… lease=…` comment present;
//! - parked execution → `parked` label present and a `phase=parked …` comment;
//! - active execution → status not `closed` and the current phase's comment.
//!
//! No writes are issued when the projection already matches, so a second
//! reconcile is a no-op (idempotent).

use agalma_contracts::{
    ContractError, ExecutionPhase, ExecutionRecord, ExecutionState, LedgerApi, TaskQueueApi,
};
use agalma_ledger::SqliteLedger;
use agalma_taskqueue::{BdTaskQueue, PARKED_LABEL};

/// Outcome counters for one reconcile pass.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct ReconcileSummary {
    /// Tasks examined (those with a ledger execution).
    pub checked: u32,
    /// Tasks whose projection was rewritten (at least one correction).
    pub fixed: u32,
    /// Individual projection corrections applied (status/label/comment drift).
    pub stale: u32,
    /// Tasks skipped because their issue was unavailable or not a task.
    pub skipped: u32,
}

impl ReconcileSummary {
    /// One-line human summary.
    pub fn render(&self) -> String {
        format!(
            "reconcile: checked={} fixed={} stale={} skipped={}",
            self.checked, self.fixed, self.stale, self.skipped
        )
    }
}

/// A single correction to apply to an issue's projection.
enum Correction {
    Status(String),
    AddLabel(String),
    RemoveLabel(String),
    Comment(String),
}

/// Reconcile every task that has a ledger execution against its bd projection.
pub fn reconcile_projection(
    ledger: &SqliteLedger,
    queue: &mut BdTaskQueue,
) -> Result<ReconcileSummary, ContractError> {
    let mut summary = ReconcileSummary::default();
    for record in latest_per_task(ledger)? {
        summary.checked += 1;
        let projection = match queue.projection(&record.task_id) {
            Ok(projection) => projection,
            Err(err) => {
                eprintln!(
                    "agalma: reconcile skip {}: issue unavailable: {err}",
                    record.task_id
                );
                summary.skipped += 1;
                continue;
            }
        };

        let lease = lease_for(ledger, &record)?;
        let corrections = corrections_for(&record, lease, &projection);
        if corrections.is_empty() {
            continue; // already matches: no writes.
        }
        for correction in corrections {
            match correction {
                Correction::Status(status) => queue.set_status(&record.task_id, &status)?,
                Correction::AddLabel(label) => queue.add_label(&record.task_id, &label)?,
                Correction::RemoveLabel(label) => queue.remove_label(&record.task_id, &label)?,
                Correction::Comment(text) => queue.comment(&record.task_id, &text)?,
            }
            summary.stale += 1;
        }
        summary.fixed += 1;
    }
    Ok(summary)
}

/// The latest (highest-generation) execution per task, ordered by task id.
fn latest_per_task(ledger: &SqliteLedger) -> Result<Vec<ExecutionRecord>, ContractError> {
    let mut latest: Vec<ExecutionRecord> = Vec::new();
    for record in ledger.executions()? {
        match latest.iter_mut().find(|r| r.task_id == record.task_id) {
            Some(existing) => {
                if record.generation > existing.generation {
                    *existing = record;
                }
            }
            None => latest.push(record),
        }
    }
    latest.sort_by(|a, b| a.task_id.cmp(&b.task_id));
    Ok(latest)
}

/// The lease generation pinned in the execution's `execution_created` event
/// (fallback: its task generation).
fn lease_for(ledger: &SqliteLedger, record: &ExecutionRecord) -> Result<u32, ContractError> {
    for event in ledger.events(&record.execution_id)? {
        if event.kind == "execution_created" {
            if let Some(lease) = event
                .payload
                .get("lease_generation")
                .and_then(|v| v.as_u64())
            {
                return Ok(lease as u32);
            }
            return Ok(record.generation);
        }
    }
    Ok(record.generation)
}

fn corrections_for(
    record: &ExecutionRecord,
    lease: u32,
    projection: &agalma_taskqueue::IssueProjection,
) -> Vec<Correction> {
    let mut corrections = Vec::new();
    let done = matches!(record.state, ExecutionState::Completed)
        || matches!(record.phase, ExecutionPhase::Done);
    let parked = matches!(record.state, ExecutionState::Parked)
        || matches!(record.phase, ExecutionPhase::Parked);

    if done {
        if projection.status == "in_progress" {
            corrections.push(Correction::Status("closed".to_string()));
        }
        if projection.has_label(PARKED_LABEL) {
            corrections.push(Correction::RemoveLabel(PARKED_LABEL.to_string()));
        }
        let required = phase_comment("done", record, lease);
        if !projection.has_comment(&required) {
            corrections.push(Correction::Comment(required));
        }
    } else if parked {
        if !projection.has_label(PARKED_LABEL) {
            corrections.push(Correction::AddLabel(PARKED_LABEL.to_string()));
        }
        let required = phase_comment("parked", record, lease);
        if !projection.has_comment(&required) {
            corrections.push(Correction::Comment(required));
        }
    } else {
        if projection.status == "closed" {
            corrections.push(Correction::Status("in_progress".to_string()));
        }
        let required = phase_comment(phase_str(record.phase), record, lease);
        if !projection.has_comment(&required) {
            corrections.push(Correction::Comment(required));
        }
    }
    corrections
}

fn phase_comment(phase: &str, record: &ExecutionRecord, lease: u32) -> String {
    format!(
        "phase={phase} execution={} lease={lease}",
        record.execution_id
    )
}

fn phase_str(phase: ExecutionPhase) -> &'static str {
    match phase {
        ExecutionPhase::Intake => "intake",
        ExecutionPhase::Checkout => "checkout",
        ExecutionPhase::Build => "build",
        ExecutionPhase::Verify => "verify",
        ExecutionPhase::Integrate => "integrate",
        ExecutionPhase::Done => "done",
        ExecutionPhase::Parked => "parked",
    }
}
