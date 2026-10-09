//! Durable worker-process identity and survivor reconciliation (M0.8).
//!
//! A confined worker (the OpenCode `serve` process for `build`, or the
//! scripted stand-in in tests) is recorded on disk the moment it is launched so
//! that a conductor restart can find it. The record carries the process id, its
//! process group, the phase, the attempt, and the operation id.
//!
//! On boot the executor probes each pending operation. For a phase with a
//! recorded worker this module either:
//!
//! - terminates a live survivor through [`SandboxApi`] and writes the returned
//!   [`StopEvidence`] to `<run>/stop/<phase>-<attempt>.json` (the proof required
//!   before any redispatch), or
//! - reports that the worker died without completing, which the caller treats
//!   as an ambiguous effect and parks.
//!
//! Completion markers (`<run>/<phase>/<attempt>.done`) are written when an
//! activity records its outcome; their presence is the "effect present" signal.

use std::path::{Path, PathBuf};

use agalma_contracts::ids::AttemptId;
use agalma_contracts::sandbox::{SandboxApi, SandboxChild};
use agalma_contracts::ExecutionId;
use agalma_execution::{ActivityError, ActivityOutcome};
use serde::de::DeserializeOwned;
use serde::{Deserialize, Serialize};

use crate::phases::run_root;

/// Durable identity of a launched worker process.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct WorkerRecord {
    pub pid: u32,
    pub pgid: u32,
    pub attempt: u32,
    pub phase: String,
    pub operation_id: String,
    pub started_at_unix_ms: u64,
}

impl WorkerRecord {
    /// Reconstruct the sandbox handle used to probe/terminate the process.
    pub fn child(&self) -> SandboxChild {
        SandboxChild {
            pid: self.pid,
            pgid: self.pgid,
            attempt: AttemptId::derive(self.attempt),
            started_at_unix_ms: self.started_at_unix_ms,
        }
    }
}

/// Result of reconciling a phase's recorded worker.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Survivor {
    /// No worker record and no prior reconciliation.
    NoRecord,
    /// A survivor was live and has just been terminated (evidence written).
    Terminated,
    /// A prior reconciliation already recorded stop evidence.
    AlreadyReconciled,
    /// A worker record exists but the process is gone without completing.
    Died,
}

/// `<run>/<phase>` directory holding the phase's markers.
fn phase_dir(state_dir: &Path, execution: &ExecutionId, phase: &str) -> PathBuf {
    run_root(state_dir, execution).join(phase)
}

/// `<run>/<phase>/<attempt>.started`.
pub fn started_path(
    state_dir: &Path,
    execution: &ExecutionId,
    phase: &str,
    attempt: u32,
) -> PathBuf {
    phase_dir(state_dir, execution, phase).join(format!("{attempt}.started"))
}

/// `<run>/<phase>/<attempt>.done`.
pub fn done_path(state_dir: &Path, execution: &ExecutionId, phase: &str, attempt: u32) -> PathBuf {
    phase_dir(state_dir, execution, phase).join(format!("{attempt}.done"))
}

/// `<run>/<phase>/<attempt>.worker.json`.
pub fn worker_path(
    state_dir: &Path,
    execution: &ExecutionId,
    phase: &str,
    attempt: u32,
) -> PathBuf {
    phase_dir(state_dir, execution, phase).join(format!("{attempt}.worker.json"))
}

/// `<run>/<phase>/<attempt>.reconciled` — set once an attempt's survivor has been
/// terminated, so a second probe does not park the operation as ambiguous.
pub fn reconciled_path(
    state_dir: &Path,
    execution: &ExecutionId,
    phase: &str,
    attempt: u32,
) -> PathBuf {
    phase_dir(state_dir, execution, phase).join(format!("{attempt}.reconciled"))
}

/// `<run>/stop/<phase>-<attempt>.json`.
pub fn stop_path(state_dir: &Path, execution: &ExecutionId, phase: &str, attempt: u32) -> PathBuf {
    run_root(state_dir, execution)
        .join("stop")
        .join(format!("{phase}-{attempt}.json"))
}

/// Write JSON to `path`, creating parent directories.
pub fn write_json<T: Serialize>(path: &Path, value: &T) -> Result<(), ActivityError> {
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)
            .map_err(|e| ActivityError::KnownFailure(format!("mkdir {}: {e}", parent.display())))?;
    }
    let text = serde_json::to_string_pretty(value)
        .map_err(|e| ActivityError::KnownFailure(format!("serialize {}: {e}", path.display())))?;
    std::fs::write(path, text)
        .map_err(|e| ActivityError::KnownFailure(format!("write {}: {e}", path.display())))
}

/// Read JSON from `path`; `None` when absent or unparseable.
pub fn read_json<T: DeserializeOwned>(path: &Path) -> Option<T> {
    let text = std::fs::read_to_string(path).ok()?;
    serde_json::from_str(&text).ok()
}

/// Record a freshly launched worker.
pub fn record_worker(
    state_dir: &Path,
    execution: &ExecutionId,
    phase: &str,
    attempt: u32,
    operation_id: &str,
    pid: u32,
    pgid: u32,
) -> Result<(), ActivityError> {
    let record = WorkerRecord {
        pid,
        pgid,
        attempt,
        phase: phase.to_string(),
        operation_id: operation_id.to_string(),
        started_at_unix_ms: now_ms(),
    };
    write_json(&worker_path(state_dir, execution, phase, attempt), &record)
}

/// Remove a worker record (on clean completion or after termination).
pub fn clear_worker(state_dir: &Path, execution: &ExecutionId, phase: &str, attempt: u32) {
    let _ = std::fs::remove_file(worker_path(state_dir, execution, phase, attempt));
}

/// Write the phase's completion marker carrying the recorded outcome.
pub fn write_done(
    state_dir: &Path,
    execution: &ExecutionId,
    phase: &str,
    attempt: u32,
    outcome: &ActivityOutcome,
) -> Result<(), ActivityError> {
    write_json(&done_path(state_dir, execution, phase, attempt), outcome)
}

/// Read a phase's recorded completion outcome.
pub fn read_done(
    state_dir: &Path,
    execution: &ExecutionId,
    phase: &str,
    attempt: u32,
) -> Option<ActivityOutcome> {
    read_json(&done_path(state_dir, execution, phase, attempt))
}

/// Whether the phase's started marker exists.
pub fn started_exists(
    state_dir: &Path,
    execution: &ExecutionId,
    phase: &str,
    attempt: u32,
) -> bool {
    started_path(state_dir, execution, phase, attempt).exists()
}

/// Clear markers left by a previous attempt before re-executing a phase.
///
/// Stop evidence (`<run>/stop/...`) is deliberately preserved: it is the durable
/// proof of survivor termination.
pub fn clear_markers(state_dir: &Path, execution: &ExecutionId, phase: &str, attempt: u32) {
    let _ = std::fs::remove_file(reconciled_path(state_dir, execution, phase, attempt));
    let _ = std::fs::remove_file(done_path(state_dir, execution, phase, attempt));
    let _ = std::fs::remove_file(started_path(state_dir, execution, phase, attempt));
}

/// Reconcile a phase's recorded worker against the sandbox.
///
/// A live survivor is terminated through `sandbox` (evidence written to
/// `<run>/stop/<phase>-<attempt>.json`) and its record removed. A worker that is
/// already gone but never completed is reported as [`Survivor::Died`].
pub fn reconcile_survivor(
    state_dir: &Path,
    execution: &ExecutionId,
    phase: &str,
    attempt: u32,
    sandbox: &mut dyn SandboxApi,
) -> Result<Survivor, ActivityError> {
    let Some(record): Option<WorkerRecord> =
        read_json(&worker_path(state_dir, execution, phase, attempt))
    else {
        // No live record: has this attempt's survivor already been reconciled?
        if reconciled_path(state_dir, execution, phase, attempt).exists() {
            return Ok(Survivor::AlreadyReconciled);
        }
        return Ok(Survivor::NoRecord);
    };

    let child = record.child();
    let alive = sandbox.alive(&child).map_err(|e| {
        ActivityError::KnownFailure(format!("probe worker pid {}: {e}", record.pid))
    })?;
    if !alive {
        return Ok(Survivor::Died);
    }

    let evidence = sandbox.terminate(&child).map_err(|e| {
        ActivityError::KnownFailure(format!("terminate worker pid {}: {e}", record.pid))
    })?;
    let payload = serde_json::json!({
        "phase": phase,
        "attempt": attempt,
        "operation_id": record.operation_id,
        "pid": record.pid,
        "pgid": record.pgid,
        "process_group_gone": evidence.process_group_gone,
        "detail": evidence.detail,
        "terminated_at_unix_ms": now_ms(),
    });
    write_json(&stop_path(state_dir, execution, phase, attempt), &payload)?;
    write_json(
        &reconciled_path(state_dir, execution, phase, attempt),
        &serde_json::json!({ "phase": phase, "attempt": attempt, "pid": record.pid }),
    )?;
    clear_worker(state_dir, execution, phase, attempt);
    if !evidence.process_group_gone {
        return Err(ActivityError::KnownFailure(format!(
            "survivor pid {} could not be fully terminated: {}",
            record.pid, evidence.detail
        )));
    }
    Ok(Survivor::Terminated)
}

fn now_ms() -> u64 {
    use std::time::{SystemTime, UNIX_EPOCH};
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_millis() as u64)
        .unwrap_or(0)
}
