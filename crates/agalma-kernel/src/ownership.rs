//! Ownership and fencing: lease generations, worker records, and evidence.
//!
//! `docs/b0-kernel.md` §"Cancellation and ownership": lease generations fence
//! stale workers; reclaiming a job requires reconciliation of prior processes
//! (evidence recorded, [`agalma_contracts::ProcessApi`] termination). This module
//! holds the kernel-native ownership types; the engine
//! ([`crate::engine::KernelEngine`]) owns the durable worker registry and the
//! fencing gate.
//!
//! Worker records and termination evidence are persisted through the durable
//! `kernel_meta` key/value store (schema v3), so a conductor restart can find a
//! prior process, confirm it is gone, and only then re-dispatch — never
//! guessing whether a stale worker is still running.

use agalma_contracts::{
    AttemptId, ExecutionId, ExecutionPhase, OperationId, ProcessChild, StopEvidence, TaskId,
};
use serde::{Deserialize, Serialize};

/// Durable `kernel_meta` key holding the JSON array of workers for an execution.
pub fn workers_key(execution_id: &ExecutionId) -> String {
    format!("workers:{}", execution_id.as_str())
}

/// Durable `kernel_meta` key holding the JSON array of termination evidence.
pub fn stop_evidence_key(execution_id: &ExecutionId) -> String {
    format!("stop_evidence:{}", execution_id.as_str())
}

/// Durable `kernel_meta` key holding the JSON array of fenced stale-lease
/// rejections for a task.
pub fn lease_fence_key(task: &TaskId) -> String {
    format!("lease_fence:{}", task.as_str())
}

/// A durable worker-process identity carrying the lease generation it launched
/// under (M1 machinery retained; see `docs/b0-kernel.md`).
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct WorkerRecord {
    pub execution_id: ExecutionId,
    pub operation_id: OperationId,
    pub phase: ExecutionPhase,
    pub attempt: u32,
    /// Lease generation of the claim that launched this worker.
    pub lease_generation: u32,
    pub pid: u32,
    pub pgid: u32,
    pub started_at_unix_ms: u64,
}

impl WorkerRecord {
    /// Reconstruct the process handle used to probe/terminate the process.
    pub fn child(&self) -> ProcessChild {
        ProcessChild {
            pid: self.pid,
            pgid: self.pgid,
            attempt: AttemptId::derive(self.attempt),
            started_at_unix_ms: self.started_at_unix_ms,
        }
    }
}

/// Recorded cleanup evidence for one terminated (or confirmed-gone) worker.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct TerminationRecord {
    pub worker: WorkerRecord,
    pub evidence: StopEvidence,
    pub recorded_at_unix_ms: u64,
}

/// A stale-lease rejection recorded by the kernel fence.
///
/// A delivery carrying a lease older than the task's current lease is fenced:
/// the execution does not advance and no effect runs; the rejection is recorded
/// here for audit (kernel-side; history is untouched).
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct StaleLease {
    /// The fenced command's signal id (operation id, directive id, timer id).
    pub id: String,
    /// The superseded lease carried by the rejected command.
    pub lease: u32,
    /// The task's current lease generation at rejection time.
    pub current: u32,
    pub recorded_at_unix_ms: u64,
}

/// Result of reclaiming a task: a new lease, prior-process reconciliation, and
/// stale-intent settlement.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Reclaim {
    /// The lease generation before reclaim (the prior claim).
    pub previous_lease: u32,
    /// The new lease generation. Equals `previous_lease` when reclaim was
    /// blocked (reconciliation could not confirm termination).
    pub new_lease: u32,
    /// Termination evidence for each reconciled prior process, in order.
    pub terminations: Vec<TerminationRecord>,
    /// Stale-lease dispatch intents released so they cannot redispatch.
    pub settled_intents: Vec<OperationId>,
    /// Set when reclaim could not confirm termination: the execution must park
    /// rather than dispatch under a fresh lease.
    pub blocked_reason: Option<String>,
}

impl Reclaim {
    /// Whether the reclaim succeeded (a fresh lease is usable).
    pub fn is_reclaimed(&self) -> bool {
        self.blocked_reason.is_none()
    }
}
