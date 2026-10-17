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
use crate::digest::{canonical_digest, canonical_json, validate_digest, FailureCode};
use crate::error::ContractError;
use crate::execution::{ExecutionPhase, ExecutionState};
use crate::ids::{ArtifactRef, ExecutionId, OperationId, TaskId};
use crate::plugin::{
    COMPONENT_CHECKPOINT_LIMIT_BYTES, COMPONENT_COMMIT_MAX_ROWS, COMPONENT_STORE_CAPABILITY,
};
use crate::snapshot::StateSnapshot;
use crate::task_package::BindingPin;

/// The revision-1 component commit format identifier.
pub const COMPONENT_COMMIT_FORMAT: &str = "ComponentCommit/v1";
/// Basename of the component commit's format family.
pub const COMPONENT_COMMIT_FAMILY: &str = "ComponentCommit";

/// Lifecycle of a durable component operation.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum OperationState {
    /// Journaled and accepted, not yet running.
    Accepted,
    /// Executing.
    Running,
    /// Finished successfully (terminal).
    Completed,
    /// Finished with a known failure (terminal).
    Failed,
    /// Cancellation requested; settlement pending.
    CancelRequested,
    /// Cancelled (terminal).
    Cancelled,
    /// Outcome could not be confirmed; reconcile before retry.
    Unknown,
}

/// One durable component event, unique by `event_id`.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ComponentEvent {
    pub event_id: String,
    pub kind: String,
    pub payload: Value,
    pub recorded_at_unix_ms: u64,
}

/// One durable component operation: journaled intent plus pinned binding/state.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ComponentOperation {
    pub operation_id: String,
    pub input_digest: String,
    pub binding: BindingPin,
    pub intent: Value,
    pub state: OperationState,
    pub deadline_unix_ms: u64,
}

/// One durable component operation receipt (idempotency anchor).
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ComponentReceipt {
    pub operation_id: String,
    pub input_digest: String,
    pub binding: BindingPin,
    pub outcome: Value,
    pub completed_at_unix_ms: u64,
}

/// An atomic component-namespace transaction (`ComponentCommit/v1`).
///
/// Either all rows commit or none do. `input_digest` is the canonical digest of
/// this commit excluding its own field; replaying a `commit_id` with identical
/// content returns the original receipt, while different content under that ID
/// conflicts.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ComponentCommit {
    /// Wire format; gated by [`ComponentCommit::version_gate`].
    pub format: String,
    pub namespace: String,
    pub commit_id: String,
    /// Canonical digest of this commit excluding its own field.
    pub input_digest: String,
    /// Optimistic concurrency guard (0 only for initial creation).
    pub expected_revision: u64,
    pub checkpoint: Option<Value>,
    pub events: Vec<ComponentEvent>,
    pub operations: Vec<ComponentOperation>,
    pub receipts: Vec<ComponentReceipt>,
}

impl ComponentCommit {
    /// Gate the `format` family/version.
    pub fn version_gate(&self) -> Result<(), ContractError> {
        match parse_commit_format_version(&self.format) {
            Ok(1) => Ok(()),
            Ok(other) => Err(FailureCode::UnsupportedVersion.failure(
                &self.namespace,
                format!("ComponentCommit version {other} is unsupported (expected 1)"),
            )),
            Err(reason) => Err(FailureCode::UnsupportedSchema.failure(
                &self.namespace,
                format!("unrecognized commit format: {reason}"),
            )),
        }
    }

    /// The canonical document to hash: this commit minus its own `input_digest`.
    pub fn canonical_document(&self) -> Result<Value, ContractError> {
        let mut value = serde_json::to_value(self).map_err(|e| {
            FailureCode::InvalidRecord.failure(
                &self.namespace,
                format!("cannot serialize commit: {e}"),
            )
        })?;
        if let Some(object) = value.as_object_mut() {
            object.remove("input_digest");
        }
        Ok(value)
    }

    /// Recompute the commit input digest over [`Self::canonical_document`].
    pub fn computed_input_digest(&self) -> Result<String, ContractError> {
        canonical_digest(&self.canonical_document()?)
    }

    /// Verify the recorded `input_digest` against the recomputed value.
    pub fn verify_input_digest(&self) -> Result<(), ContractError> {
        validate_digest(&self.input_digest, &self.commit_id)?;
        let recomputed = self.computed_input_digest()?;
        if recomputed != self.input_digest {
            return Err(FailureCode::IntegrityMismatch.failure(
                &self.commit_id,
                format!(
                    "commit input digest {} does not match recomputed {recomputed}",
                    self.input_digest
                ),
            ));
        }
        Ok(())
    }

    /// Refuse more than [`COMPONENT_COMMIT_MAX_ROWS`] rows of any kind, or a
    /// checkpoint whose canonical encoding exceeds
    /// [`COMPONENT_CHECKPOINT_LIMIT_BYTES`].
    pub fn check_bounds(&self) -> Result<(), ContractError> {
        for (label, count) in [
            ("events", self.events.len()),
            ("operations", self.operations.len()),
            ("receipts", self.receipts.len()),
        ] {
            if count > COMPONENT_COMMIT_MAX_ROWS {
                return Err(FailureCode::InvalidRecord.failure(
                    &self.commit_id,
                    format!(
                        "component commit has {count} {label}, exceeding {COMPONENT_COMMIT_MAX_ROWS}"
                    ),
                ));
            }
        }
        if let Some(checkpoint) = &self.checkpoint {
            let text = canonical_json(checkpoint)?;
            let bytes = text.len() as u64;
            if bytes > COMPONENT_CHECKPOINT_LIMIT_BYTES {
                return Err(FailureCode::InvalidRecord.failure(
                    &self.commit_id,
                    format!(
                        "component checkpoint {bytes} bytes exceeds limit {COMPONENT_CHECKPOINT_LIMIT_BYTES}"
                    ),
                ));
            }
        }
        Ok(())
    }
}

/// Durable per-namespace component state row.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ComponentStateRow {
    pub namespace: String,
    pub revision: u64,
    pub watermark: u64,
    pub checkpoint: Option<Value>,
    pub recorded_at_unix_ms: u64,
}

/// A durable component event row: namespace plus monotonic per-namespace
/// sequence.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ComponentEventRow {
    pub namespace: String,
    pub sequence: u64,
    pub event: ComponentEvent,
}

/// A durable component operation row.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ComponentOperationRow {
    pub namespace: String,
    pub operation: ComponentOperation,
}

/// A durable component receipt row.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ComponentReceiptRow {
    pub namespace: String,
    pub receipt: ComponentReceipt,
}

/// The durable replay anchor for one committed component transaction.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ComponentCommitAnchor {
    pub namespace: String,
    pub commit_id: String,
    pub input_digest: String,
    pub revision: u64,
    pub committed_at_unix_ms: u64,
}

impl ComponentCommitAnchor {
    /// Replay check against another anchor: the same namespace/commit id with a
    /// changed `input_digest` is [`FailureCode::IdentityConflict`]; identical
    /// content returns the original receipt; a different anchor is independent.
    pub fn check_replay(&self, other: &ComponentCommitAnchor) -> Result<(), ContractError> {
        if self.namespace == other.namespace
            && self.commit_id == other.commit_id
            && self.input_digest != other.input_digest
        {
            return Err(FailureCode::IdentityConflict.failure(
                &self.commit_id,
                format!(
                    "component commit {}/{} reused with changed input_digest",
                    self.namespace, self.commit_id
                ),
            ));
        }
        Ok(())
    }
}

/// Default refusal for a ledger binding without the negotiated component-store
/// capability.
fn component_store_unavailable() -> ContractError {
    FailureCode::MissingCapability.failure(
        COMPONENT_STORE_CAPABILITY,
        "component storage requires negotiated component-store/v1",
    )
}

/// Parse `ComponentCommit/vN` into `N`.
fn parse_commit_format_version(format: &str) -> Result<u64, String> {
    let (family, version) = format
        .split_once('/')
        .ok_or_else(|| format!("{format:?} has no `/version`"))?;
    if family != COMPONENT_COMMIT_FAMILY {
        return Err(format!("unknown family {family:?}"));
    }
    let digits = version
        .strip_prefix('v')
        .ok_or_else(|| format!("version {version:?} is not `vN`"))?;
    digits
        .parse::<u64>()
        .map_err(|_| format!("version {version:?} is not `vN`"))
}

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

/// One durable control-action receipt (schema v4 `control_receipts`).
///
/// Every interactive control action (pause/resume/cancel) records exactly one
/// receipt, applied or denied. `action_id` is the idempotency anchor: replaying
/// the same action is a no-op, and reusing an `action_id` with different
/// contents is a [`ContractError::Conflict`]. `target` names the acted-on
/// subject (an execution id for cancel; empty for the global latch). `request`
/// carries the request detail, including the `outcome` field for denied
/// attempts. Receipts are audit records only: they gate nothing and never
/// rewind Git or the ledger.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct ControlReceipt {
    /// Idempotency anchor (primary key).
    pub action_id: String,
    /// Action name: `pause` / `resume` / `cancel`.
    pub action: String,
    /// Acted-on subject: execution id for `cancel`, empty for latch actions.
    pub target: String,
    /// Who requested the action (e.g. `human`).
    pub actor: String,
    /// Request detail, including the `outcome` (`applied`/`denied`) field.
    pub request: Value,
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

    /// Record a control-action receipt (schema v4 `control_receipts`).
    ///
    /// Idempotent by `action_id`: re-recording identical contents (ignoring the
    /// timestamp) is a no-op; reusing an `action_id` with different contents is
    /// a [`ContractError::Conflict`]. Applied *and* denied attempts are
    /// receipted; the caller records the outcome in `request`.
    fn record_control_receipt(
        &mut self,
        receipt: &ControlReceipt,
    ) -> Result<(), ContractError>;

    /// All recorded control receipts, ordered by `recorded_at` then `action_id`.
    fn control_receipts(&self) -> Result<Vec<ControlReceipt>, ContractError>;

    /// Export the complete canonical state as a versioned [`StateSnapshot`].
    ///
    /// `docs/b0-kernel.md` §"Ledger schema v3": history + projection + receipts
    /// plus dispatch intents + timers + directives + bindings + kernel contract
    /// versions, producing a canonical JSON document with a format header.
    fn export_state(&self) -> Result<StateSnapshot, ContractError>;

    /// Replace all durable state with `snapshot`, atomically.
    ///
    /// Refuses a snapshot whose format header or physical `schema_version` is
    /// incompatible ([`StateSnapshot::check_compatible`]) rather than applying
    /// it partially. Preserves canonical ids, receipts, and pending work
    /// (including the consumed flag of each dispatch intent).
    fn import_state(&mut self, snapshot: &StateSnapshot) -> Result<(), ContractError>;

    /// Read one component namespace's durable state row, if any.
    ///
    /// Component storage requires the negotiated `component-store/v1`
    /// capability; a binding without it refuses before dispatch.
    fn component_read(&self, _namespace: &str) -> Result<Option<ComponentStateRow>, ContractError> {
        Err(component_store_unavailable())
    }

    /// Component events for a namespace with sequence `> after`, up to `limit`.
    fn component_events(
        &self,
        _namespace: &str,
        _after: u64,
        _limit: u32,
    ) -> Result<Vec<ComponentEventRow>, ContractError> {
        Err(component_store_unavailable())
    }

    /// One component operation row by namespace and operation id, if present.
    fn component_operation(
        &self,
        _namespace: &str,
        _operation_id: &str,
    ) -> Result<Option<ComponentOperationRow>, ContractError> {
        Err(component_store_unavailable())
    }

    /// Atomically commit a component-namespace transaction, returning its
    /// durable anchor. Replaying `commit_id` with identical content returns the
    /// original anchor; different content under that ID conflicts.
    fn component_commit(
        &mut self,
        _commit: &ComponentCommit,
    ) -> Result<ComponentCommitAnchor, ContractError> {
        Err(component_store_unavailable())
    }
}
