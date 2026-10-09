//! `LedgerApi` — canonical durable state.
//!
//! The atomic commit API accepts a typed batch of state changes, events,
//! receipts, and dispatch intents; it commits the whole batch or returns a
//! conflict/failure. Handles never expose SQL or transaction handles, and no
//! external I/O runs inside a transaction.

use serde::{Deserialize, Serialize};
use serde_json::Value;

use crate::binding::BindingRecord;
use crate::error::ContractError;
use crate::execution::{ExecutionPhase, ExecutionState};
use crate::ids::{ExecutionId, OperationId, TaskId};

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

/// Ledger contract. SQL, transactions, and the physical schema stay private.
pub trait LedgerApi {
    /// Atomically commit a batch, or return a conflict/failure.
    fn commit(&mut self, batch: CommitBatch) -> Result<CommitReceipt, ContractError>;

    /// Read one execution row.
    fn execution(&self, id: &ExecutionId) -> Result<Option<ExecutionRecord>, ContractError>;

    /// Pending (unconsumed) dispatch intents.
    fn pending_intents(&self) -> Result<Vec<DispatchIntent>, ContractError>;

    /// Events for an execution, in monotonic sequence order.
    fn events(&self, id: &ExecutionId) -> Result<Vec<ExecutionEvent>, ContractError>;

    /// Whether the kill latch is set.
    fn kill_latch(&self) -> Result<bool, ContractError>;

    /// Set or clear the kill latch.
    fn set_kill_latch(&mut self, latched: bool) -> Result<(), ContractError>;

    /// All component binding records.
    fn bindings(&self) -> Result<Vec<BindingRecord>, ContractError>;

    /// Stored schema version. A mismatched version parks, never auto-migrates.
    fn schema_version(&self) -> Result<u32, ContractError>;
}
