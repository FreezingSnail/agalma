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

/// Lifecycle of a durable timer row (schema v3 `timers`).
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub enum TimerState {
    /// Scheduled and not yet fired (cancellable).
    Pending,
    /// Fired exactly once (idempotency anchor).
    Fired,
    /// Cancelled because the owning work completed/was cancelled.
    Cancelled,
}

/// Durable retry schedule or deadline (schema v3 `timers`).
///
/// `timer_id` is the unique idempotency anchor: a recovered
/// `Command::TimerFired { timer_id }` fires at most once regardless of how many
/// restarts observe it. `payload` is bounded metadata (identifiers, hashes,
/// references) recorded with the schedule; large artifacts stay outside.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct TimerRecord {
    pub timer_id: String,
    pub execution_id: ExecutionId,
    /// `retry` / `deadline` (free-form so callers can add kinds additively).
    pub kind: String,
    pub fire_at_unix_ms: u64,
    pub state: TimerState,
    pub payload: Value,
}

/// Lifecycle of a durable directive row (schema v3 `directives`).
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub enum DirectiveState {
    /// The current active version.
    Active,
    /// Replaced by a newer version (kept for audit).
    Superseded,
    /// Cancelled by a newer directive or an explicit cancel.
    Cancelled,
}

/// Incoming directive update (schema v3 `directives`).
///
/// `directive_id` is unique: the row holds the latest version. `source` +
/// `version` bind an update to its origin so duplicates are harmless and a
/// newer version replaces an older one. `payload` carries the work plan
/// (bounded: identifiers, hashes, references).
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct DirectiveRow {
    pub directive_id: String,
    pub version: u64,
    pub source: String,
    pub state: DirectiveState,
    pub payload: Value,
    pub received_at_unix_ms: u64,
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

    /// Read a kernel contract-version key (schema v3 `kernel_meta`), if set.
    ///
    /// Kernel-versioning keys are `exec_def_version` and `kernel_version`
    /// (`docs/b0-kernel.md` §"Ledger schema v3"); the store is a small key/value
    /// table separate from the physical-schema `meta`. Timers and directives get
    /// their own methods in B0.4.
    fn kernel_meta_get(&self, key: &str) -> Result<Option<String>, ContractError>;

    /// Insert or replace a kernel contract-version key (schema v3 `kernel_meta`).
    fn kernel_meta_set(&mut self, key: &str, value: &str) -> Result<(), ContractError>;

    /// Insert a durable timer row (schema v3 `timers`), atomically.
    ///
    /// Idempotent by `timer_id`: re-inserting the identical row is a no-op
    /// returning `false`; reusing an id with different contents is a
    /// [`ContractError::Conflict`]. Returns `true` when a new row was written.
    fn insert_timer(&mut self, timer: &TimerRecord) -> Result<bool, ContractError>;

    /// One timer row by id, if present.
    fn timer(&self, timer_id: &str) -> Result<Option<TimerRecord>, ContractError>;

    /// Pending timers due at or before `now_unix_ms`, ordered by
    /// `(fire_at, timer_id)`. The recovery work list for `TimerFired`.
    fn due_timers(&self, now_unix_ms: u64) -> Result<Vec<TimerRecord>, ContractError>;

    /// Mark a pending timer fired, atomically and idempotently.
    ///
    /// A fired or cancelled timer is left unchanged (fires at most once).
    fn mark_timer_fired(&mut self, timer_id: &str) -> Result<(), ContractError>;

    /// Cancel every pending timer owned by `execution_id`, returning the count.
    ///
    /// Called when the owning work completes or is cancelled: pending timers
    /// must not fire for terminal work.
    fn cancel_timers_for_execution(
        &mut self,
        execution_id: &ExecutionId,
    ) -> Result<u64, ContractError>;

    /// Earliest pending timer deadline (`fire_at`), if any.
    ///
    /// The conductor sleeps until this deadline instead of busy-waiting.
    fn next_deadline(&self) -> Result<Option<u64>, ContractError>;

    /// Insert or replace a directive row (schema v3 `directives`), atomically.
    ///
    /// Unique by `directive_id`: a strictly newer `version` replaces the row
    /// (returns `true`); an equal-or-older version with identical contents is a
    /// harmless no-op (returns `false`); the same version with different
    /// contents is a [`ContractError::Conflict`].
    fn upsert_directive(&mut self, directive: &DirectiveRow) -> Result<bool, ContractError>;

    /// One directive row by id, if present (latest version).
    fn directive(&self, directive_id: &str) -> Result<Option<DirectiveRow>, ContractError>;

    /// Mark a dispatch intent consumed without recording a receipt, atomically.
    ///
    /// Used when the owning operation is cancelled: queued work must be
    /// released rather than left pending for recovery.
    fn consume_dispatch_intent(&mut self, operation_id: &OperationId) -> Result<(), ContractError>;
}
