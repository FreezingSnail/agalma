//! `LedgerApi` — canonical durable state.
//!
//! The atomic commit API accepts a typed batch of state changes, events,
//! receipts, and dispatch intents; it commits the whole batch or returns a
//! conflict/failure. Handles never expose SQL or transaction handles, and no
//! external I/O runs inside a transaction.

use serde::{Deserialize, Serialize};
use serde_json::Value;

use crate::binding::BindingRecord;
use crate::decision::{DecisionOutcome, DecisionRequest};
use crate::error::ContractError;
use crate::execution::{ExecutionPhase, ExecutionState};
use crate::ids::{ArtifactRef, ExecutionId, OperationId, TaskId};

/// Durable row for one execution.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct ExecutionRecord {
    pub execution_id: ExecutionId,
    pub task_id: TaskId,
    pub generation: u32,
    pub phase: ExecutionPhase,
    pub state: ExecutionState,
    pub attempt: u32,
    pub revision: u64,
}

/// One monotonic execution event (sequence is per execution).
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct ExecutionEvent {
    pub execution_id: ExecutionId,
    pub sequence: u64,
    pub kind: String,
    pub payload: Value,
    pub recorded_at_unix_ms: u64,
}

/// Recorded receipt for a completed logical operation (idempotency anchor).
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct OperationReceipt {
    pub operation_id: OperationId,
    pub execution_id: ExecutionId,
    pub kind: String,
    pub inputs_hash: String,
    pub result: Value,
    pub completed_at_unix_ms: u64,
}

/// Durable dispatch intent, recorded before the effect and consumed after.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct DispatchIntent {
    pub operation_id: OperationId,
    pub execution_id: ExecutionId,
    pub payload: Value,
    pub enqueued_at_unix_ms: u64,
    pub consumed: bool,
}

/// Atomic commit batch: state + events + receipts + intents in one transaction.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct CommitBatch {
    /// Optimistic concurrency guard; `None` skips the revision check.
    pub expected_revision: Option<u64>,
    /// New execution row state, when the phase/state/attempt changed.
    pub state: Option<ExecutionRecord>,
    pub events: Vec<ExecutionEvent>,
    pub receipts: Vec<OperationReceipt>,
    pub intents: Vec<DispatchIntent>,
}

/// Result of a committed batch.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct CommitReceipt {
    pub revision: u64,
    pub committed_at_unix_ms: u64,
}

/// One derived per-execution digest row (schema v2).
///
/// Cost/usage are derived from existing `execution_events`; this row stores the
/// derived summary plus the optional artifact path. `version` is per-execution
/// and monotonic.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct DigestRecord {
    pub execution_id: ExecutionId,
    pub version: u32,
    /// Derived summary (cost/usage, files, commands+outcomes, repairs, wall time).
    pub summary: Value,
    /// Path/reference to the serialized digest artifact, when one was written.
    pub artifact_ref: Option<ArtifactRef>,
    pub recorded_at_unix_ms: u64,
}

/// Ledger contract. SQL, transactions, and the physical schema stay private.
pub trait LedgerApi {
    /// Atomically commit a batch, or return a conflict/failure.
    fn commit(&mut self, batch: CommitBatch) -> Result<CommitReceipt, ContractError>;

    /// Read one execution row.
    fn execution(&self, id: &ExecutionId) -> Result<Option<ExecutionRecord>, ContractError>;

    /// All execution rows, ordered by execution id.
    ///
    /// Recovery needs to enumerate durable executions (including those with no
    /// pending intent) to reconstruct each from its events and compare against
    /// the projection.
    fn executions(&self) -> Result<Vec<ExecutionRecord>, ContractError>;

    /// Pending (unconsumed) dispatch intents.
    fn pending_intents(&self) -> Result<Vec<DispatchIntent>, ContractError>;

    /// Events for an execution, in monotonic sequence order.
    fn events(&self, id: &ExecutionId) -> Result<Vec<ExecutionEvent>, ContractError>;

    /// Recorded receipt for a completed operation, if any.
    ///
    /// This is the idempotency anchor: re-delivering a completed operation
    /// returns this recorded receipt instead of repeating the effect.
    fn operation_receipt(
        &self,
        id: &OperationId,
    ) -> Result<Option<OperationReceipt>, ContractError>;

    /// The task's current lease generation, or `0` when never claimed.
    ///
    /// A lease generation is a monotonic per-task counter incremented once per
    /// claim cycle ([`LedgerApi::begin_lease`]). It fences dispatch intents,
    /// operation receipts, and worker records: an operation tagged with a lease
    /// older than the task's current lease is stale and must not produce an
    /// effect.
    fn lease_generation(&self, task: &TaskId) -> Result<u32, ContractError>;

    /// Begin a new claim cycle for `task`: increment and persist its lease
    /// generation, returning the new value (starts at `1`).
    ///
    /// This is the durable half of the atomic claim gate: `bd --claim` elects a
    /// single winner, and the winner's lease generation fences every subsequent
    /// operation and worker record against re-claims.
    fn begin_lease(&mut self, task: &TaskId) -> Result<u32, ContractError>;

    /// Whether the kill latch is set.
    fn kill_latch(&self) -> Result<bool, ContractError>;

    /// Set or clear the kill latch.
    fn set_kill_latch(&mut self, latched: bool) -> Result<(), ContractError>;

    /// Insert or update a component binding record.
    fn upsert_binding(&mut self, binding: &BindingRecord) -> Result<(), ContractError>;

    /// All component binding records.
    fn bindings(&self) -> Result<Vec<BindingRecord>, ContractError>;

    /// Stored schema version. A mismatched version parks, never auto-migrates.
    fn schema_version(&self) -> Result<u32, ContractError>;

    /// Persist a decision request before dispatch (schema v2 `decisions`).
    ///
    /// Idempotent for identical contents; reusing an operation ID with changed
    /// contents is a conflict (the caller must use a new operation ID).
    fn record_decision(&mut self, request: &DecisionRequest) -> Result<(), ContractError>;

    /// Persist the validated outcome for an operation (before scheduling
    /// dependent work). A recorded outcome is terminal and never overwritten.
    fn record_decision_outcome(
        &mut self,
        operation_id: &OperationId,
        outcome: &DecisionOutcome,
    ) -> Result<(), ContractError>;

    /// Recorded decision outcome for an operation, if any (recovery reuse).
    fn decision_outcome(
        &self,
        operation_id: &OperationId,
    ) -> Result<Option<DecisionOutcome>, ContractError>;

    /// Insert or replace a derived digest row (schema v2 `digests`).
    fn record_digest(&mut self, digest: &DigestRecord) -> Result<(), ContractError>;

    /// Digest rows for an execution, ordered by `version`.
    fn digests_for_execution(
        &self,
        execution_id: &ExecutionId,
    ) -> Result<Vec<DigestRecord>, ContractError>;
}
